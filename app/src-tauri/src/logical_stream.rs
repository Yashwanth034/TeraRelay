use actix_web::{web::Bytes, Error};
use futures::{Stream, StreamExt};

#[derive(Debug, PartialEq, Eq)]
pub(super) struct PartRange {
    pub index: usize,
    pub start: u64,
    pub length: u64,
}

pub(super) fn part_ranges(sizes: &[u64], start: u64, end: u64) -> Option<Vec<PartRange>> {
    let total = sizes
        .iter()
        .try_fold(0u64, |sum, size| sum.checked_add(*size))?;
    if start > end || end >= total {
        return None;
    }
    let mut offset = 0;
    let mut ranges = Vec::new();
    for (index, size) in sizes.iter().copied().enumerate() {
        if size == 0 {
            continue;
        }
        let next = offset + size;
        if start < next && end >= offset {
            let local_start = start.saturating_sub(offset);
            let local_end = end.min(next - 1) - offset;
            ranges.push(PartRange {
                index,
                start: local_start,
                length: local_end - local_start + 1,
            });
        }
        offset = next;
    }
    Some(ranges)
}

pub(super) fn bounded_chunks<S, E>(
    source: S,
    mut skip: u64,
    mut remaining: Option<u64>,
) -> impl Stream<Item = Result<Bytes, Error>>
where
    S: Stream<Item = Result<Vec<u8>, E>>,
    E: std::fmt::Display,
{
    async_stream::stream! {
        if remaining == Some(0) {
            return;
        }
        futures::pin_mut!(source);
        while let Some(chunk) = source.next().await {
            let data = match chunk {
                Ok(data) => data,
                Err(error) => {
                    yield Err(actix_web::error::ErrorBadGateway(error.to_string()));
                    return;
                }
            };
            let leading = skip.min(data.len() as u64) as usize;
            skip -= leading as u64;
            let available = data.len() - leading;
            let length = remaining.map(|left| left.min(available as u64) as usize).unwrap_or(available);
            if length > 0 {
                yield Ok(Bytes::from(data).slice(leading..leading + length));
                if let Some(left) = remaining.as_mut() {
                    *left -= length as u64;
                    if *left == 0 {
                        return;
                    }
                }
            }
        }
        if skip > 0 || remaining.is_some_and(|left| left > 0) {
            yield Err(actix_web::error::ErrorBadGateway("Media part ended before the requested range"));
        }
    }
}

pub(super) fn range_response<F, S>(
    sizes: &[u64],
    mime: &str,
    req: &actix_web::HttpRequest,
    source: F,
) -> actix_web::HttpResponse
where
    F: FnOnce(Vec<PartRange>) -> S,
    S: Stream<Item = Result<Bytes, Error>> + 'static,
{
    use actix_web::HttpResponse;
    let Some(total) = sizes
        .iter()
        .try_fold(0u64, |sum, size| sum.checked_add(*size))
    else {
        return HttpResponse::BadGateway().body("Video size overflow");
    };
    let requested = req.headers().get(actix_web::http::header::RANGE);
    let (start, end) = if let Some(range) = requested {
        match range
            .to_str()
            .ok()
            .and_then(|range| super::parse_range_header(range, total))
        {
            Some(range) => range,
            None => {
                return HttpResponse::RangeNotSatisfiable()
                    .insert_header(("Content-Range", format!("bytes */{}", total)))
                    .insert_header(("Accept-Ranges", "bytes"))
                    .finish()
            }
        }
    } else {
        (0, total.saturating_sub(1))
    };
    let ranges = if total == 0 {
        Vec::new()
    } else {
        part_ranges(sizes, start, end).expect("validated range")
    };
    let mut response = if requested.is_some() {
        let mut response = HttpResponse::PartialContent();
        response.insert_header((
            "Content-Range",
            format!("bytes {}-{}/{}", start, end, total),
        ));
        response
    } else {
        HttpResponse::Ok()
    };
    response.insert_header((
        "Content-Length",
        if total == 0 { 0 } else { end - start + 1 }.to_string(),
    ));
    response.insert_header(("Content-Type", mime.to_string()));
    response.insert_header(("Accept-Ranges", "bytes"));
    response.insert_header(("Cache-Control", "private, max-age=120"));
    response.streaming(source(ranges))
}

pub(super) type PartKey = (bool, i64, i32);
#[derive(Clone)]
pub(super) struct CachedParts {
    pub ids: Vec<i32>,
    pub metadata: Vec<(u64, String)>,
    created: std::time::Instant,
}
#[derive(Default)]
pub(super) struct PartCache {
    entries: std::sync::Mutex<std::collections::HashMap<PartKey, CachedParts>>,
}
impl PartCache {
    pub fn get(&self, key: PartKey) -> Option<CachedParts> {
        let mut entries = self.entries.lock().ok()?;
        entries.retain(|_, parts| parts.created.elapsed() < std::time::Duration::from_secs(300));
        entries.get(&key).cloned()
    }
    pub fn insert(&self, key: PartKey, ids: Vec<i32>, metadata: Vec<(u64, String)>) {
        if let Ok(mut entries) = self.entries.lock() {
            if entries.len() >= 128 && !entries.contains_key(&key) {
                if let Some(oldest) = entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.created)
                    .map(|(key, _)| *key)
                {
                    entries.remove(&oldest);
                }
            }
            entries.insert(
                key,
                CachedParts {
                    ids,
                    metadata,
                    created: std::time::Instant::now(),
                },
            );
        }
    }
    pub fn invalidate(&self, key: PartKey) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(&key);
        }
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    #[test]
    fn repeated_ranges_reuse_parts_and_changed_media_invalidates_them() {
        let cache = PartCache::default();
        let key = (false, 99, 7);
        let metadata = vec![
            (4, "clip.mp4.tgdpart001-002".to_string()),
            (3, "clip.mp4.tgdpart002-002".to_string()),
        ];
        cache.insert(key, vec![7, 8], metadata.clone());
        assert_eq!(cache.get(key).unwrap().ids, [7, 8]);
        assert_eq!(cache.get(key).unwrap().metadata, metadata);
        assert!(
            cache.get((false, 100, 7)).is_none(),
            "Different Saved Messages account reused parts"
        );
        assert!(
            cache.get((true, 99, 7)).is_none(),
            "Channel and user IDs were conflated"
        );
        cache.invalidate(key);
        assert!(
            cache.get(key).is_none(),
            "Changed/deleted parts remained cached"
        );
    }
}

#[cfg(test)]
mod response_tests {
    use super::*;

    #[actix_web::test]
    async fn response_headers_and_bytes_describe_the_whole_logical_video() {
        let request = actix_web::test::TestRequest::default()
            .insert_header(("Range", "bytes=3-11"))
            .to_http_request();
        let response = range_response(&[5, 4, 6], "video/mp4", &request, |ranges| {
            let parts = [
                b"abcde".as_slice(),
                b"fghi".as_slice(),
                b"jklmno".as_slice(),
            ];
            let bytes: Vec<u8> = ranges
                .into_iter()
                .flat_map(|range| {
                    parts[range.index][range.start as usize..(range.start + range.length) as usize]
                        .to_vec()
                })
                .collect();
            futures::stream::iter([Ok::<_, actix_web::Error>(Bytes::from(bytes))])
        });
        assert_eq!(
            response.status(),
            actix_web::http::StatusCode::PARTIAL_CONTENT
        );
        assert_eq!(
            response.headers().get("Content-Range").unwrap(),
            "bytes 3-11/15"
        );
        assert_eq!(response.headers().get("Content-Length").unwrap(), "9");
        assert_eq!(
            actix_web::body::to_bytes(response.into_body())
                .await
                .unwrap()
                .as_ref(),
            b"defghijkl"
        );
    }

    #[actix_web::test]
    async fn invalid_ranges_do_not_start_a_download() {
        let request = actix_web::test::TestRequest::default()
            .insert_header(("Range", "bytes=15-"))
            .to_http_request();
        let response = range_response(&[5, 4, 6], "video/mp4", &request, |_| {
            panic!("An unsatisfiable range fetched the video");
            #[allow(unreachable_code)]
            futures::stream::empty::<Result<Bytes, actix_web::Error>>()
        });
        assert_eq!(
            response.status(),
            actix_web::http::StatusCode::RANGE_NOT_SATISFIABLE
        );
        assert_eq!(
            response.headers().get("Content-Range").unwrap(),
            "bytes */15"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{stream, StreamExt};

    #[test]
    fn ranges_cross_part_boundaries_without_restarting_file_offsets() {
        assert_eq!(
            part_ranges(&[5, 4, 6], 3, 11).unwrap(),
            vec![
                PartRange {
                    index: 0,
                    start: 3,
                    length: 2
                },
                PartRange {
                    index: 1,
                    start: 0,
                    length: 4
                },
                PartRange {
                    index: 2,
                    start: 0,
                    length: 3
                },
            ]
        );
        assert_eq!(
            part_ranges(&[5, 4, 6], 9, 14).unwrap(),
            vec![PartRange {
                index: 2,
                start: 0,
                length: 6
            }]
        );
        assert!(part_ranges(&[5, 4], 9, 9).is_none());
        assert!(part_ranges(&[u64::MAX, 1], 0, 1).is_none());
    }

    #[actix_web::test]
    async fn exact_bytes_survive_transport_chunk_and_file_part_boundaries() {
        let data: [&[u8]; 3] = [b"abcde", b"fghi", b"jklmno"];
        let mut actual = Vec::new();
        for part in part_ranges(&[5, 4, 6], 3, 11).unwrap() {
            let chunks: Vec<Result<Vec<u8>, &'static str>> =
                data[part.index].chunks(2).map(|c| Ok(c.to_vec())).collect();
            let bytes = bounded_chunks(stream::iter(chunks), part.start, Some(part.length));
            futures::pin_mut!(bytes);
            while let Some(chunk) = bytes.next().await {
                actual.extend_from_slice(&chunk.unwrap());
            }
        }
        assert_eq!(actual, b"defghijkl");
    }

    #[actix_web::test]
    async fn truncated_media_is_an_error_not_a_successful_short_body() {
        let source = stream::iter(vec![Ok::<_, &'static str>(b"short".to_vec())]);
        let bytes = bounded_chunks(source, 0, Some(8));
        futures::pin_mut!(bytes);
        assert_eq!(&bytes.next().await.unwrap().unwrap()[..], b"short");
        assert!(bytes.next().await.unwrap().is_err());
    }

    #[actix_web::test]
    async fn zero_length_ranges_do_not_fetch_transport_data() {
        let source = stream::iter(vec![Err::<Vec<u8>, _>("must not fetch")]);
        let bytes = bounded_chunks(source, 0, Some(0));
        futures::pin_mut!(bytes);
        assert!(bytes.next().await.is_none());
    }
}
