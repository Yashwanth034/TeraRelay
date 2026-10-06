from __future__ import annotations

import ctypes
import hashlib
import json
import sys
import time
from collections import deque
from pathlib import Path


def emit(payload: dict) -> None:
    sys.stdout.write(json.dumps(payload, separators=(",", ":")) + "\n")
    sys.stdout.flush()


class TDJson:
    def __init__(self, tdjson_path: str, sqlcipher_path: str | None):
        self._deps: list[object] = []
        mode = getattr(ctypes, "RTLD_GLOBAL", 0)
        if sqlcipher_path:
            self._deps.append(ctypes.CDLL(sqlcipher_path, mode=mode))
        self.lib = ctypes.CDLL(tdjson_path, mode=mode)

        self.lib.td_create_client_id.argtypes = []
        self.lib.td_create_client_id.restype = ctypes.c_int
        self.lib.td_send.argtypes = [ctypes.c_int, ctypes.c_char_p]
        self.lib.td_send.restype = None
        self.lib.td_receive.argtypes = [ctypes.c_double]
        self.lib.td_receive.restype = ctypes.c_char_p
        self.lib.td_execute.argtypes = [ctypes.c_char_p]
        self.lib.td_execute.restype = ctypes.c_char_p

        if hasattr(self.lib, "td_set_log_verbosity_level"):
            self.lib.td_set_log_verbosity_level.argtypes = [ctypes.c_int]
            self.lib.td_set_log_verbosity_level.restype = None
            self.lib.td_set_log_verbosity_level(0)

        self.client_id = int(self.lib.td_create_client_id())
        self.extra = 0
        self.updates: deque[dict] = deque()

    def send(self, request: dict) -> None:
        payload = json.dumps(request, separators=(",", ":")).encode("utf-8")
        self.lib.td_send(self.client_id, payload)

    def receive(self, timeout: float = 0.5) -> dict | None:
        raw = self.lib.td_receive(max(0.0, float(timeout)))
        if not raw:
            return None
        obj = json.loads(raw.decode("utf-8"))
        client_id = obj.get("@client_id")
        if client_id is not None and int(client_id) != self.client_id:
            return None
        return obj

    def request(self, request: dict, timeout: float = 30.0) -> dict:
        self.extra += 1
        token = f"terarelay-{self.client_id}-{self.extra}"
        payload = dict(request)
        payload["@extra"] = token
        self.send(payload)

        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            obj = self.receive(min(0.5, max(0.01, deadline - time.monotonic())))
            if obj is None:
                continue
            if obj.get("@extra") == token:
                if obj.get("@type") == "error":
                    raise RuntimeError(
                        f"TDLib error {obj.get('code')}: {obj.get('message', 'unknown error')}"
                    )
                return obj
            self.updates.append(obj)
        raise TimeoutError(f"Timed out waiting for {request.get('@type', 'TDLib request')}")

    def pop_update(self) -> dict | None:
        return self.updates.popleft() if self.updates else None


class Worker:
    def __init__(self, cfg: dict):
        self.api_id = int(cfg["api_id"])
        self.api_hash = str(cfg["api_hash"])
        self.db_dir = Path(cfg["db_dir"])
        self.files_dir = Path(cfg["files_dir"])
        self.db_key = str(cfg["db_key"])
        self.db_dir.mkdir(parents=True, exist_ok=True)
        self.files_dir.mkdir(parents=True, exist_ok=True)
        try:
            self.db_dir.chmod(0o700)
            self.files_dir.chmod(0o700)
        except OSError:
            pass

        self.td = TDJson(str(cfg["tdjson"]), cfg.get("sqlcipher"))
        self.auth_state = "starting"
        self.ready = False
        self.qr_link = None

    def parameters(self) -> dict:
        return {
            "@type": "setTdlibParameters",
            "use_test_dc": False,
            "database_directory": str(self.db_dir),
            "files_directory": str(self.files_dir),
            "database_encryption_key": self.db_key,
            "use_file_database": True,
            "use_chat_info_database": True,
            "use_message_database": False,
            "use_secret_chats": False,
            "api_id": self.api_id,
            "api_hash": self.api_hash,
            "system_language_code": "en",
            "device_model": "TeraRelay Desktop",
            "system_version": sys.platform,
            "application_version": "0.1.0",
        }

    def network_file_totals(self) -> tuple[int, int] | None:
        """Return cumulative TDLib file-traffic bytes for this isolated worker.

        TDLib's updateFile callbacks are deliberately coalesced and can be
        several seconds apart. Network statistics are maintained independently
        and let the UI reflect real file traffic between those callbacks.
        """
        try:
            stats = self.td.request(
                {"@type": "getNetworkStatistics", "only_current": True},
                timeout=2,
            )
        except Exception:
            return None

        sent = 0
        received = 0
        for entry in stats.get("entries") or []:
            if not isinstance(entry, dict):
                continue
            if entry.get("@type") != "networkStatisticsEntryFile":
                continue
            sent += max(0, int(entry.get("sent_bytes") or 0))
            received += max(0, int(entry.get("received_bytes") or 0))
        return sent, received

    def advance_auth(self, timeout: float = 25.0) -> str:
        if self.ready:
            return "ready"

        # A previous status/prepare request may already have consumed the current
        # wait-state update. Reuse that cached state immediately instead of
        # waiting for TDLib to emit the same authorization state again.
        if self.auth_state == "authorizationStateWaitPhoneNumber":
            return "phone"
        if self.auth_state == "authorizationStateWaitCode":
            return "code"
        if self.auth_state == "authorizationStateWaitPassword":
            return "password"
        if self.auth_state == "authorizationStateWaitOtherDeviceConfirmation":
            return "qr"
        if self.auth_state in {
            "authorizationStateClosing",
            "authorizationStateClosed",
            "authorizationStateLoggingOut",
        }:
            self.ready = False
            return "closed"

        if self.auth_state == "starting":
            self.td.send({"@type": "getOption", "name": "version"})

        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            obj = self.td.pop_update() or self.td.receive(
                min(0.5, max(0.01, deadline - time.monotonic()))
            )
            if not obj or obj.get("@type") != "updateAuthorizationState":
                continue

            state = obj.get("authorization_state", {})
            state_type = state.get("@type", "unknown")
            self.auth_state = state_type

            if state_type == "authorizationStateWaitTdlibParameters":
                self.td.request(self.parameters(), timeout=30)
                continue
            if state_type == "authorizationStateWaitEncryptionKey":
                self.td.request(
                    {
                        "@type": "checkDatabaseEncryptionKey",
                        "encryption_key": self.db_key,
                    },
                    timeout=30,
                )
                continue
            if state_type == "authorizationStateWaitPhoneNumber":
                return "phone"
            if state_type == "authorizationStateWaitCode":
                return "code"
            if state_type == "authorizationStateWaitPassword":
                return "password"
            if state_type == "authorizationStateWaitOtherDeviceConfirmation":
                self.qr_link = str(state.get("link") or "")
                return "qr"
            if state_type == "authorizationStateReady":
                self.ready = True
                self.qr_link = None
                return "ready"
            if state_type == "authorizationStateWaitRegistration":
                raise RuntimeError("Existing Telegram accounts only")
            if state_type in {
                "authorizationStateWaitEmailAddress",
                "authorizationStateWaitEmailCode",
                "authorizationStateWaitPremiumPurchase",
            }:
                raise RuntimeError(f"Unsupported Telegram authorization step: {state_type}")
            if state_type in {
                "authorizationStateClosing",
                "authorizationStateClosed",
                "authorizationStateLoggingOut",
            }:
                self.ready = False
                return "closed"

        raise TimeoutError(
            f"Timed out waiting for TDLib authorization state after {self.auth_state}"
        )

    def start_qr_auth(self) -> dict:
        state = self.advance_auth(15)
        if state == "ready":
            return {"auth_state": "ready", "ready": True, "link": None}
        if state != "phone":
            raise RuntimeError(f"TDLib QR authorization expected phone state, got {state}")

        self.td.request(
            {"@type": "requestQrCodeAuthentication", "other_user_ids": []},
            timeout=30,
        )
        self.auth_state = "qr_requested"
        state = self.advance_auth(30)
        if state == "ready":
            return {"auth_state": "ready", "ready": True, "link": None}
        if state != "qr" or not self.qr_link:
            raise RuntimeError(f"TDLib did not provide a QR login token; state={state}")
        return {"auth_state": "qr", "ready": False, "link": self.qr_link}

    def wait_qr_auth(self) -> dict:
        if self.ready:
            return {"auth_state": "ready", "ready": True}
        self.auth_state = "qr_accepted"
        state = self.advance_auth(45)
        return {"auth_state": state, "ready": state == "ready"}

    def set_phone(self, value: str) -> str:
        self.td.request(
            {
                "@type": "setAuthenticationPhoneNumber",
                "phone_number": value.strip(),
                "settings": None,
            },
            timeout=30,
        )
        # The cached state is still WaitPhoneNumber until TDLib emits the
        # transition. Do not let advance_auth() reuse that stale pre-submit
        # state or the UI will remain on the phone form even though Telegram
        # already sent the login code.
        self.auth_state = "phone_submitted"
        return self.advance_auth(30)

    def set_code(self, value: str) -> str:
        self.td.request(
            {"@type": "checkAuthenticationCode", "code": value.strip()},
            timeout=30,
        )
        # Same rule as phone submission: wait for the new authorization state
        # instead of returning the cached WaitCode state.
        self.auth_state = "code_submitted"
        return self.advance_auth(30)

    def set_password(self, value: str) -> str:
        self.td.request(
            {"@type": "checkAuthenticationPassword", "password": value},
            timeout=30,
        )
        self.auth_state = "password_submitted"
        return self.advance_auth(30)

    def _find_loaded_supergroup_chat(self, supergroup_id: int) -> int | None:
        """Find a TDLib chat ID from updateNewChat events without dropping updates."""
        retained = deque()
        found = None
        while self.td.updates:
            obj = self.td.updates.popleft()
            retained.append(obj)
            if obj.get("@type") != "updateNewChat":
                continue
            chat = obj.get("chat") or {}
            chat_type = chat.get("type") or {}
            if (
                chat_type.get("@type") == "chatTypeSupergroup"
                and int(chat_type.get("supergroup_id", -1)) == int(supergroup_id)
            ):
                found = int(chat["id"])
        self.td.updates.extend(retained)
        return found

    def _load_supergroup_chat(self, supergroup_id: int) -> int:
        """Load this account's chat lists until the requested private channel is known."""
        # TDLib's createSupergroupChat only works after the supergroup metadata is
        # known to this TDLib database. A fresh TDLib authorization may know zero
        # chats even though the same Telegram account already has the channel.
        found = self._find_loaded_supergroup_chat(supergroup_id)
        if found is not None:
            return found

        for chat_list in ("chatListMain", "chatListArchive"):
            for _ in range(5):
                try:
                    self.td.request(
                        {
                            "@type": "loadChats",
                            "chat_list": {"@type": chat_list},
                            "limit": 100,
                        },
                        timeout=30,
                    )
                except RuntimeError as exc:
                    # TDLib returns an error when that list has no more chats.
                    message = str(exc).lower()
                    if "no more chats" in message or "404" in message:
                        break
                    raise

                found = self._find_loaded_supergroup_chat(supergroup_id)
                if found is not None:
                    return found

                # Once loaded, createSupergroupChat is the canonical way to
                # materialize a chat object for a known supergroup.
                try:
                    chat = self.td.request(
                        {
                            "@type": "createSupergroupChat",
                            "supergroup_id": int(supergroup_id),
                            "force": False,
                        },
                        timeout=20,
                    )
                    return int(chat["id"])
                except RuntimeError as exc:
                    if "chat info not found" not in str(exc).lower():
                        raise

        raise RuntimeError(
            f"TDLib could not resolve Telegram channel {supergroup_id} after loading chat lists"
        )

    def destination_chat(self, destination: dict) -> int:
        kind = destination.get("kind")
        if kind == "saved":
            me = self.td.request({"@type": "getMe"}, timeout=15)
            user_id = int(me["id"])
            chat = self.td.request(
                {
                    "@type": "createPrivateChat",
                    "user_id": user_id,
                    "force": False,
                },
                timeout=15,
            )
            return int(chat["id"])

        if kind == "channel":
            raw_id = int(destination["id"])
            try:
                chat = self.td.request(
                    {
                        "@type": "createSupergroupChat",
                        "supergroup_id": raw_id,
                        "force": False,
                    },
                    timeout=20,
                )
                return int(chat["id"])
            except RuntimeError as exc:
                if "chat info not found" not in str(exc).lower():
                    raise
            return self._load_supergroup_chat(raw_id)

        raise RuntimeError(f"Unsupported TDLib destination: {kind}")

    def upload(self, request_id: int, request: dict) -> dict:
        if not self.ready:
            state = self.advance_auth(15)
            if state != "ready":
                raise RuntimeError(f"TDLib session is not authorized ({state})")

        path = Path(str(request["path"])).expanduser().resolve()
        if not path.is_file():
            raise RuntimeError(f"Upload source does not exist: {path}")

        size = path.stat().st_size
        if size <= 0:
            raise RuntimeError(f"Telegram cannot upload an empty file: {path.name}")

        destination = request.get("destination") or {"kind": "saved"}
        caption = str(request.get("caption") or "")
        chat_id = self.destination_chat(destination)

        from typing import Callable

        last_emitted = 0
        confirmed = 0
        last_heartbeat = time.monotonic()
        td = self.td

        def emit_upload_progress(candidate: int, allow_complete: bool = False) -> None:
            nonlocal last_emitted
            upper = size if allow_complete or size <= 1 else size - 1
            bounded = min(max(0, int(candidate)), upper)
            if bounded <= last_emitted:
                return
            delta = bounded - last_emitted
            last_emitted = bounded
            emit({
                "event": "progress",
                "id": request_id,
                "delta": delta,
                "uploaded": bounded,
                "size": size,
            })

        def on_bytes(amount: int) -> None:
            nonlocal confirmed
            delta = min(max(0, int(amount)), size - confirmed)
            if delta <= 0:
                return
            confirmed += delta
            emit_upload_progress(confirmed)
            emit({
                "event": "network",
                "id": request_id,
                "delta": delta,
                "measurement": "acknowledged",
            })

        class UploadEvents:
            def read_event(proxy, event):
                nonlocal last_heartbeat
                now = time.monotonic()
                if now - last_heartbeat >= 0.25:
                    emit({"event": "heartbeat", "id": request_id})
                    last_heartbeat = now
                if event and event.get("@type") == "updateSpeedLimitNotification":
                    is_upload = event.get("is_upload")
                    if isinstance(is_upload, bool):
                        emit({"event": "speed_limit", "id": request_id,
                              "is_upload": is_upload})
                return event

            def request(proxy, payload, timeout=30):
                payload["input_message_content"]["caption"]["text"] = caption
                return td.request(payload, timeout=timeout)

            def pop_update(proxy):
                return proxy.read_event(td.pop_update())

            def receive(proxy, timeout=0.5):
                return proxy.read_event(td.receive(min(0.25, timeout)))

        class NativeUpload:
            td = UploadEvents()
            ready = self.ready

            def send_document(
                self,
                chat_id: int,
                path: Path,
                on_bytes: Callable[[int], None] | None = None,
                timeout: float = 7200.0,
            ) -> str | None:
                """Send one general file through TDLib and report real upload-byte deltas."""
                path = path.expanduser().resolve()
                if not path.is_file():
                    raise RuntimeError(f"Telegram file does not exist: {path}")
                size = path.stat().st_size
                if size <= 0:
                    raise RuntimeError(f"Telegram cannot upload an empty file: {path.name}")
                if not self.ready:
                    raise RuntimeError("TDLib session is not authorized.")

                sent = self.td.request(
                    {
                        "@type": "sendMessage",
                        "chat_id": int(chat_id),
                        "message_thread_id": 0,
                        "reply_to": None,
                        "options": None,
                        "reply_markup": None,
                        "input_message_content": {
                            "@type": "inputMessageDocument",
                            "document": {"@type": "inputFileLocal", "path": str(path)},
                            "thumbnail": None,
                            "disable_content_type_detection": True,
                            "caption": {"@type": "formattedText", "text": "", "entities": []},
                        },
                    },
                    timeout=30,
                )
                old_message_id = int(sent.get("id", 0))
                content = sent.get("content", {})
                document = content.get("document", {}) if isinstance(content, dict) else {}
                file_obj = document.get("document", {}) if isinstance(document, dict) else {}
                file_id = int(file_obj["id"]) if isinstance(file_obj, dict) and file_obj.get("id") is not None else None

                last_uploaded = 0
                deadline = time.monotonic() + timeout
                while time.monotonic() < deadline:
                    obj = self.td.pop_update() or self.td.receive(min(0.5, max(0.01, deadline - time.monotonic())))
                    if not obj:
                        continue
                    typ = obj.get("@type")
                    if typ == "updateFile":
                        current = obj.get("file", {})
                        if file_id is not None and int(current.get("id", -1)) != file_id:
                            continue
                        remote = current.get("remote", {})
                        uploaded = max(0, int(remote.get("uploaded_size", 0) or 0))
                        if uploaded > last_uploaded:
                            delta = min(uploaded, size) - min(last_uploaded, size)
                            last_uploaded = uploaded
                            if delta > 0 and on_bytes:
                                on_bytes(delta)
                        continue
                    if typ == "updateMessageSendSucceeded":
                        old = obj.get("old_message_id")
                        if old is None or int(old) == old_message_id:
                            # TDLib may coalesce the final updateFile with message success.
                            # Count only any unreported tail, never more than the file size.
                            if on_bytes and last_uploaded < size:
                                on_bytes(size - last_uploaded)
                            message = obj.get("message", {})
                            remote_id = int(message.get("id", 0))
                            return str(remote_id) if remote_id else None
                        continue
                    if typ == "updateMessageSendFailed":
                        old = obj.get("old_message_id")
                        if old is None or int(old) == old_message_id:
                            error = obj.get("error", {})
                            raise RuntimeError(f"TDLib send failed: {error.get('message', 'unknown error')}")
                raise TimeoutError(f"TDLib upload timed out: {path.name}")

        message_id = NativeUpload().send_document(
            chat_id,
            path,
            on_bytes=on_bytes,
            timeout=float(request.get("timeout", 7200.0)),
        )
        emit_upload_progress(size, allow_complete=True)
        return {
            "message_id": int(message_id or 0),
            "bytes_uploaded": size,
        }

    def download(self, request_id: int, request: dict) -> dict:
        if not self.ready:
            state = self.advance_auth(15)
            if state != "ready":
                raise RuntimeError(f"TDLib session is not authorized ({state})")

        destination = request.get("destination") or {"kind": "saved"}
        chat_id = self.destination_chat(destination)
        chunks = request.get("chunks") or []
        if not chunks:
            raise RuntimeError("TDLib download request has no chunks")

        output_path = Path(str(request["path"])).expanduser().resolve()
        output_path.parent.mkdir(parents=True, exist_ok=True)
        force = bool(request.get("force", False))
        inactivity_timeout = float(request.get("inactivity_timeout", request.get("timeout", 7200.0)))

        tracked: dict[int, dict] = {}
        total_expected = 0
        cached_at_start = 0
        for chunk in chunks:
            server_message_id = int(chunk["message_id"])
            expected_size = int(chunk.get("size") or 0)
            expected_hash = str(chunk.get("sha256") or "")
            td_message_id = server_message_id << 20
            message = self.td.request(
                {
                    "@type": "getMessage",
                    "chat_id": chat_id,
                    "message_id": td_message_id,
                },
                timeout=30,
            )
            content = message.get("content") or {}
            if content.get("@type") != "messageDocument":
                raise RuntimeError(
                    f"Stored chunk message {server_message_id} is not a Telegram document"
                )
            document = content.get("document") or {}
            file_obj = document.get("document") or {}
            file_id = int(file_obj.get("id") or 0)
            if file_id <= 0:
                raise RuntimeError(
                    f"Stored chunk message {server_message_id} has no downloadable file"
                )

            if expected_size <= 0:
                expected_size = int(file_obj.get("size") or 0)
            if expected_size <= 0:
                raise RuntimeError(
                    f"Stored chunk message {server_message_id} has no known file size"
                )

            # A freshly uploaded split part can remain marked by TDLib as
            # "downloaded" at the temporary upload path even after TeraRelay
            # has correctly removed that temporary file. Treat only this
            # impossible state (completed + missing local file) as stale local
            # cache metadata. Resetting it with deleteFile affects TDLib's
            # local cache only; the remote Telegram document is untouched.
            initial_local = file_obj.get("local") or {}
            initial_path = str(initial_local.get("path") or "")
            stale_completed_local = (
                bool(initial_local.get("is_downloading_completed"))
                and bool(initial_path)
                and not Path(initial_path).expanduser().is_file()
            )

            initial_downloaded = min(
                max(0, int(initial_local.get("downloaded_size") or 0)),
                expected_size,
            )

            if force or stale_completed_local:
                try:
                    self.td.request({"@type": "deleteFile", "file_id": file_id}, timeout=20)
                except Exception as exc:
                    if stale_completed_local:
                        raise RuntimeError(
                            f"Failed to reset stale TDLib local file state for chunk "
                            f"{server_message_id}: {exc}"
                        ) from exc
                initial_downloaded = 0

            tracked[file_id] = {
                "message_id": server_message_id,
                "size": expected_size,
                "sha256": expected_hash,
                "downloaded": initial_downloaded,
                "completed": False,
                "path": "",
            }
            cached_at_start += initial_downloaded
            total_expected += expected_size

        network_received_total = 0
        last_emitted = 0

        def authoritative_downloaded_total() -> int:
            return min(
                total_expected,
                sum(max(0, int(info["downloaded"])) for info in tracked.values()),
            )

        def emit_download_progress(candidate: int, allow_complete: bool = False) -> None:
            nonlocal last_emitted
            upper = total_expected if allow_complete or total_expected <= 1 else total_expected - 1
            bounded = min(max(0, int(candidate)), upper)
            if bounded <= last_emitted:
                return
            delta = bounded - last_emitted
            received_delta = max(0, bounded - max(last_emitted, cached_at_start))
            last_emitted = bounded
            emit_network_bytes(received_delta)
            emit(
                {
                    "event": "progress",
                    "id": request_id,
                    "delta": delta,
                    "downloaded": bounded,
                    "size": total_expected,
                }
            )

        def emit_network_bytes(delta: int) -> None:
            nonlocal network_received_total
            if delta <= 0:
                return
            network_received_total += int(delta)
            emit(
                {
                    "event": "network",
                    "id": request_id,
                    "delta": int(delta),
                }
            )

        emit_download_progress(cached_at_start)
        started = time.monotonic()
        last_progress_at = started

        for file_id, info in tracked.items():
            result = self.td.request(
                {
                    "@type": "downloadFile",
                    "file_id": file_id,
                    "priority": 32,
                    "offset": 0,
                    "limit": 0,
                    "synchronous": False,
                },
                timeout=30,
            )
            local = result.get("local") or {}
            candidate = str(local.get("path") or "")
            stale_completed_path = (
                bool(local.get("is_downloading_completed"))
                and bool(candidate)
                and not Path(candidate).expanduser().is_file()
            )

            # TDLib may remember the temporary source path used for an upload
            # and report that missing path as an already-completed download.
            # Reset only that concrete stale path, then start the real remote
            # download in this same request instead of returning an error that
            # requires the user to press Retry.
            if stale_completed_path:
                self.td.request(
                    {"@type": "deleteFile", "file_id": file_id},
                    timeout=20,
                )
                result = self.td.request(
                    {
                        "@type": "downloadFile",
                        "file_id": file_id,
                        "priority": 32,
                        "offset": 0,
                        "limit": 0,
                        "synchronous": False,
                    },
                    timeout=30,
                )
                local = result.get("local") or {}
                candidate = str(local.get("path") or "")

            downloaded = min(
                int(local.get("downloaded_size") or 0),
                int(info["size"]),
            )
            if downloaded > int(info["downloaded"]):
                info["downloaded"] = downloaded
                last_progress_at = time.monotonic()
            emit_download_progress(authoritative_downloaded_total())

            if (
                bool(local.get("is_downloading_completed"))
                and candidate
                and Path(candidate).expanduser().is_file()
            ):
                info["completed"] = True
                info["path"] = candidate

        last_progress_poll = time.monotonic()
        last_heartbeat = started
        pending_progress: dict[int, float] = {}
        progress_tag = f"download-progress-{request_id}-"

        def heartbeat() -> None:
            nonlocal last_heartbeat
            now = time.monotonic()
            if now - last_heartbeat >= 0.25:
                emit({"event": "heartbeat", "id": request_id})
                last_heartbeat = now

        while not all(bool(info["completed"]) for info in tracked.values()):
            now = time.monotonic()
            # File size must not impose a wall-clock limit while confirmed bytes
            # keep arriving. Heartbeats and unchanged/unrelated snapshots do not
            # extend this inactivity budget.
            deadline = last_progress_at + inactivity_timeout
            if now >= deadline:
                raise TimeoutError("TDLib download timed out")
            heartbeat()

            if now - last_progress_poll >= 0.25:
                # Keep receiving file updates while snapshots are in flight.
                for tracked_file_id, tracked_info in tracked.items():
                    if bool(tracked_info["completed"]):
                        continue
                    pending_since = pending_progress.get(tracked_file_id)
                    if pending_since is not None and now - pending_since < 2.0:
                        continue
                    self.td.send({
                        "@type": "getFile",
                        "file_id": tracked_file_id,
                        "@extra": f"{progress_tag}{tracked_file_id}",
                    })
                    pending_progress[tracked_file_id] = now
                last_progress_poll = now

            obj = self.td.pop_update() or self.td.receive(
                min(0.1, max(0.01, deadline - time.monotonic()))
            )
            if not obj:
                continue
            if obj.get("@type") == "updateSpeedLimitNotification":
                is_upload = obj.get("is_upload")
                if isinstance(is_upload, bool):
                    emit({"event": "speed_limit", "id": request_id, "is_upload": is_upload})
                continue
            if obj.get("@type") == "updateFile":
                current = obj.get("file") or {}
            elif obj.get("@type") == "file":
                current = obj
                response_file_id = int(current.get("id") or 0)
                if obj.get("@extra") != f"{progress_tag}{response_file_id}":
                    continue
                pending_progress.pop(response_file_id, None)
            else:
                continue

            file_id = int(current.get("id") or 0)
            info = tracked.get(file_id)
            if info is None:
                continue

            local = current.get("local") or {}
            bounded_current = min(
                int(local.get("downloaded_size") or 0),
                int(info["size"]),
            )
            if bounded_current > int(info["downloaded"]):
                info["downloaded"] = bounded_current
                last_progress_at = time.monotonic()
            emit_download_progress(authoritative_downloaded_total())

            if bool(local.get("is_downloading_completed")):
                info["completed"] = True
                info["path"] = str(local.get("path") or "")

        network_elapsed = max(0.001, time.monotonic() - started)

        # TDLib can emit is_downloading_completed just before the final local
        # path/file becomes visible to the client. Refresh only those completed
        # entries for a short bounded window instead of failing the whole
        # logical download immediately. Integrity checks below remain strict.
        unresolved = {
            file_id
            for file_id, info in tracked.items()
            if not str(info.get("path") or "")
            or not Path(str(info.get("path") or "")).expanduser().is_file()
        }
        settle_deadline = time.monotonic() + 3.0
        re_requested = set()
        while unresolved and time.monotonic() < settle_deadline:
            for file_id in list(unresolved):
                try:
                    current = self.td.request(
                        {"@type": "getFile", "file_id": file_id},
                        timeout=2,
                    )
                    local = current.get("local") or {}
                    candidate = str(local.get("path") or "")
                    if candidate:
                        tracked[file_id]["path"] = candidate
                    if (
                        bool(local.get("is_downloading_completed"))
                        and candidate
                        and Path(candidate).expanduser().is_file()
                    ):
                        tracked[file_id]["completed"] = True
                        unresolved.discard(file_id)
                        continue

                    # Mirror the manual Retry behavior once when TDLib says the
                    # download is complete but the local file/path has not
                    # materialized yet. This is intentionally non-forcing so
                    # valid cached bytes are preserved.
                    if file_id not in re_requested:
                        re_requested.add(file_id)
                        self.td.request(
                            {
                                "@type": "downloadFile",
                                "file_id": file_id,
                                "priority": 32,
                                "offset": 0,
                                "limit": 0,
                                "synchronous": False,
                            },
                            timeout=2,
                        )
                except Exception:
                    # Keep waiting within the bounded settle window. The
                    # reconstruction phase below will surface a hard failure if
                    # the file never becomes available.
                    pass

            if unresolved:
                time.sleep(0.1)

        if unresolved:
            unresolved_messages = [
                int(tracked[file_id]["message_id"])
                for file_id in sorted(unresolved)
            ]
            raise RuntimeError(
                "TDLib completed chunk(s) without a local file after settle: "
                + ", ".join(str(message_id) for message_id in unresolved_messages)
            )

        with output_path.open("wb") as output:
            reconstructed = 0
            for chunk in chunks:
                server_message_id = int(chunk["message_id"])
                matched = next(
                    info
                    for info in tracked.values()
                    if int(info["message_id"]) == server_message_id
                )
                local_path = Path(str(matched["path"])).expanduser().resolve()
                if not local_path.is_file():
                    raise RuntimeError(
                        f"TDLib completed chunk {server_message_id} without a local file"
                    )

                expected_size = int(matched["size"])
                actual_size = local_path.stat().st_size
                if actual_size != expected_size:
                    raise RuntimeError(
                        f"Downloaded chunk {server_message_id} size mismatch: expected "
                        f"{expected_size}, got {actual_size}"
                    )

                expected_hash = str(matched.get("sha256") or "")
                hasher = hashlib.sha256() if expected_hash else None
                copied = 0
                with local_path.open("rb") as source:
                    while True:
                        block = source.read(4 * 1024 * 1024)
                        if not block:
                            break
                        output.write(block)
                        heartbeat()
                        copied += len(block)
                        reconstructed += len(block)
                        if hasher is not None:
                            hasher.update(block)

                if copied != expected_size:
                    raise RuntimeError(
                        f"Downloaded chunk {server_message_id} reconstruction was incomplete"
                    )
                if hasher is not None and hasher.hexdigest() != expected_hash:
                    raise RuntimeError(
                        f"Downloaded chunk {server_message_id} failed SHA-256 verification"
                    )

            output.flush()

        if reconstructed != total_expected:
            try:
                output_path.unlink()
            except OSError:
                pass
            raise RuntimeError(
                f"Reconstructed file size mismatch: expected {total_expected}, got {reconstructed}"
            )

        emit_download_progress(total_expected, allow_complete=True)
        measured_network_bytes = network_received_total
        if measured_network_bytes <= 0:
            # Statistics may be unavailable on an older/runtime-specific TDLib
            # build. Fall back to bytes that actually had to come from network,
            # excluding bytes already present in TDLib's cache at start.
            measured_network_bytes = max(0, total_expected - cached_at_start)

        return {
            "bytes_downloaded": reconstructed,
            "network_seconds": network_elapsed,
            "average_bytes_per_sec": int(measured_network_bytes / network_elapsed),
        }

    def logout(self) -> dict:
        if self.ready:
            self.td.request({"@type": "logOut"}, timeout=30)
            self.ready = False
            self.auth_state = "logout_submitted"
            try:
                state = self.advance_auth(20)
            except Exception:
                state = "closed"
        else:
            state = self.auth_state
        return {"logged_out": True, "auth_state": state}

    def close(self) -> None:
        try:
            self.td.request({"@type": "close"}, timeout=5)
        except Exception:
            pass
        self.ready = False


def main() -> int:
    config_line = sys.stdin.readline()
    if not config_line:
        return 2

    try:
        cfg = json.loads(config_line)
        worker = Worker(cfg)
        emit({"event": "started"})
    except Exception as exc:
        emit({"event": "fatal", "error": str(exc)})
        return 1

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue

        request_id = 0
        try:
            request = json.loads(line)
            request_id = int(request.get("id", 0) or 0)
            action = request.get("action")

            if action in {"status", "prepare"}:
                state = worker.advance_auth(float(request.get("timeout", 15)))
                is_premium = None
                if state == "ready":
                    me = worker.td.request({"@type": "getMe"}, timeout=15)
                    is_premium = bool(me.get("is_premium", False))
                result = {
                    "auth_state": state,
                    "ready": state == "ready",
                    "is_premium": is_premium,
                }
            elif action == "qr_start":
                result = worker.start_qr_auth()
            elif action == "qr_wait":
                result = worker.wait_qr_auth()
            elif action == "phone":
                state = worker.set_phone(str(request.get("phone") or ""))
                result = {"auth_state": state, "ready": state == "ready"}
            elif action == "code":
                state = worker.set_code(str(request.get("code") or ""))
                result = {"auth_state": state, "ready": state == "ready"}
            elif action == "password":
                state = worker.set_password(str(request.get("password") or ""))
                result = {"auth_state": state, "ready": state == "ready"}
            elif action == "upload":
                result = worker.upload(request_id, request)
            elif action == "download":
                result = worker.download(request_id, request)
            elif action == "logout":
                result = worker.logout()
            elif action == "close":
                worker.close()
                result = {"closed": True}
            elif action == "shutdown":
                worker.close()
                emit({"event": "result", "id": request_id, "ok": True, "result": {"closed": True}})
                return 0
            else:
                raise RuntimeError(f"Unknown worker action: {action}")

            emit(
                {
                    "event": "result",
                    "id": request_id,
                    "ok": True,
                    "result": result,
                }
            )
        except Exception as exc:
            emit(
                {
                    "event": "result",
                    "id": request_id,
                    "ok": False,
                    "error": str(exc),
                }
            )

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
