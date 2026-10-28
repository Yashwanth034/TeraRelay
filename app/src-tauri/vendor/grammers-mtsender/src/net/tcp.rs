// Copyright 2020 - developers of the `grammers` project.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Modified for TeraRelay: adapt to hickory-resolver 0.26.1 without resolver panics,
// use the OS resolver on iOS, and add local SOCKS5 regression tests. See ../../VENDOR.md.
use log::info;
use tokio::net::TcpStream;
pub use tokio::net::tcp::{ReadHalf, WriteHalf};

use super::ServerAddr;

pub enum NetStream {
    Tcp(TcpStream),
    #[cfg(feature = "proxy")]
    ProxySocks5(tokio_socks::tcp::Socks5Stream<TcpStream>),
}

impl NetStream {
    pub(crate) fn split(&mut self) -> (ReadHalf<'_>, WriteHalf<'_>) {
        match self {
            Self::Tcp(stream) => stream.split(),
            #[cfg(feature = "proxy")]
            Self::ProxySocks5(stream) => stream.split(),
        }
    }

    pub(crate) async fn connect(addr: &ServerAddr) -> Result<Self, std::io::Error> {
        info!("connecting...");
        match addr {
            ServerAddr::Tcp { address } => Ok(NetStream::Tcp(TcpStream::connect(address).await?)),
            #[cfg(feature = "proxy")]
            ServerAddr::Proxied { address, proxy } => {
                Self::connect_proxy_stream(address, proxy).await
            }
        }
    }

    #[cfg(feature = "proxy")]
    async fn connect_proxy_stream(
        addr: &std::net::SocketAddr,
        proxy_url: &str,
    ) -> Result<NetStream, std::io::Error> {
        use std::{
            io::{self, ErrorKind},
            net::{IpAddr, SocketAddr},
        };

        #[cfg(not(target_os = "ios"))]
        use hickory_resolver::Resolver;
        use url::Host;

        let proxy = url::Url::parse(proxy_url)
            .map_err(|err| io::Error::new(ErrorKind::InvalidData, err))?;
        let scheme = proxy.scheme();
        let host = proxy.host().ok_or(io::Error::new(
            ErrorKind::NotFound,
            format!("proxy host is missing from url: {}", proxy_url),
        ))?;
        let port = proxy.port().ok_or(io::Error::new(
            ErrorKind::NotFound,
            format!("proxy port is missing from url: {}", proxy_url),
        ))?;
        let username = proxy.username();
        let password = proxy.password().unwrap_or("");
        let socks_addr = match host {
            Host::Domain(domain) => {
                #[cfg(not(target_os = "ios"))]
                {
                    let resolver = Resolver::builder_tokio()
                        .map_err(io::Error::other)?
                        .build()
                        .map_err(io::Error::other)?;
                    let response = resolver.lookup_ip(domain).await.map_err(io::Error::other)?;
                    let socks_ip_addr = response.iter().next().ok_or(io::Error::new(
                        ErrorKind::NotFound,
                        format!("proxy host did not return any ip address: {}", domain),
                    ))?;
                    SocketAddr::new(socks_ip_addr, port)
                }
                #[cfg(target_os = "ios")]
                {
                    // Resolve only the proxy endpoint. The destination still travels
                    // through SOCKS5; DNS failure must never trigger a direct route.
                    tokio::net::lookup_host((domain, port))
                        .await?
                        .next()
                        .ok_or_else(|| {
                            io::Error::new(
                                ErrorKind::NotFound,
                                format!("proxy host did not return any ip address: {}", domain),
                            )
                        })?
                }
            }
            Host::Ipv4(v4) => SocketAddr::new(IpAddr::from(v4), port),
            Host::Ipv6(v6) => SocketAddr::new(IpAddr::from(v6), port),
        };

        match scheme {
            "socks5" => {
                if username.is_empty() {
                    Ok(NetStream::ProxySocks5(
                        tokio_socks::tcp::Socks5Stream::connect(socks_addr, addr)
                            .await
                            .map_err(|err| io::Error::new(ErrorKind::ConnectionAborted, err))?,
                    ))
                } else {
                    Ok(NetStream::ProxySocks5(
                        tokio_socks::tcp::Socks5Stream::connect_with_password(
                            socks_addr, addr, username, password,
                        )
                        .await
                        .map_err(|err| io::Error::new(ErrorKind::ConnectionAborted, err))?,
                    ))
                }
            }
            scheme => Err(io::Error::new(
                ErrorKind::ConnectionAborted,
                format!("proxy scheme not supported: {}", scheme),
            )),
        }
    }
}

#[cfg(all(test, feature = "proxy"))]
mod tests {
    use super::NetStream;
    use std::{io::ErrorKind, net::SocketAddr, time::Duration};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    // A local SOCKS peer checks the wire handshake, then echoes one payload.
    // No Telegram connection or external DNS is involved.
    async fn loopback_proxy(password_auth: bool, hostname: bool) {
        let bind_addr = if hostname {
            tokio::net::lookup_host(("localhost", 0))
                .await
                .unwrap()
                .next()
                .unwrap()
        } else {
            "127.0.0.1:0".parse().unwrap()
        };
        let listener = TcpListener::bind(bind_addr).await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0; 2];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting[0], 5);
            let mut methods = vec![0; greeting[1] as usize];
            stream.read_exact(&mut methods).await.unwrap();
            let method = if password_auth { 2 } else { 0 };
            assert!(methods.contains(&method));
            stream.write_all(&[5, method]).await.unwrap();
            if password_auth {
                let mut header = [0; 2];
                stream.read_exact(&mut header).await.unwrap();
                assert_eq!(header, [1, 4]);
                let mut user = [0; 4];
                stream.read_exact(&mut user).await.unwrap();
                assert_eq!(&user, b"user");
                let len = stream.read_u8().await.unwrap();
                assert_eq!(len, 4);
                let mut pass = [0; 4];
                stream.read_exact(&mut pass).await.unwrap();
                assert_eq!(&pass, b"pass");
                stream.write_all(&[1, 0]).await.unwrap();
            }
            let mut request = [0; 10];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(request, [5, 1, 0, 1, 127, 0, 0, 1, 1, 187]);
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                .await
                .unwrap();
            let byte = stream.read_u8().await.unwrap();
            stream.write_all(&[byte]).await.unwrap();
        });
        let credentials = if password_auth { "user:pass@" } else { "" };
        let proxy_host = if hostname {
            format!("localhost:{}", proxy_addr.port())
        } else {
            proxy_addr.to_string()
        };
        let target: SocketAddr = "127.0.0.1:443".parse().unwrap();
        let mut stream = NetStream::connect_proxy_stream(
            &target,
            &format!("socks5://{credentials}{proxy_host}"),
        )
        .await
        .unwrap();
        let (mut read, mut write) = stream.split();
        write.write_all(&[42]).await.unwrap();
        assert_eq!(read.read_u8().await.unwrap(), 42);
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn socks5_loopback_without_authentication() {
        tokio::time::timeout(Duration::from_secs(5), loopback_proxy(false, false))
            .await
            .expect("SOCKS handshake timed out");
    }

    #[tokio::test]
    async fn socks5_loopback_with_password_authentication() {
        tokio::time::timeout(Duration::from_secs(5), loopback_proxy(true, false))
            .await
            .expect("SOCKS handshake timed out");
    }

    #[cfg(target_os = "ios")]
    #[tokio::test]
    async fn socks5_loopback_with_os_resolved_hostname() {
        tokio::time::timeout(Duration::from_secs(5), loopback_proxy(true, true))
            .await
            .expect("hostname SOCKS handshake timed out");
    }

    #[tokio::test]
    async fn invalid_proxy_urls_return_errors() {
        let target: SocketAddr = "127.0.0.1:443".parse().unwrap();
        for (url, kind) in [
            ("not a url", ErrorKind::InvalidData),
            ("socks5://127.0.0.1", ErrorKind::NotFound),
            ("http://127.0.0.1:1080", ErrorKind::ConnectionAborted),
        ] {
            match NetStream::connect_proxy_stream(&target, url).await {
                Err(err) => assert_eq!(err.kind(), kind, "{url}"),
                Ok(_) => panic!("invalid proxy URL was accepted: {url}"),
            }
        }
    }
}
