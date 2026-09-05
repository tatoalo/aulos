//! `GET <p>download/*` and `<p>audio_download/*`: ranges, conditionals, containment and the JSON
//! listing (PROTOCOL §4.7, DESIGN §16.6), under both prefixes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use support::{Rig, for_each_prefix};

const BODY: &[u8] = b"0123456789abcdefghijABCDEFGHIJ";

#[tokio::test]
async fn a_whole_file_comes_back_with_the_documented_headers() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        rig.write_download("A video.mp4", BODY);

        let response = rig.get_raw("download/A%20video.mp4").await;
        assert_eq!(response.status().as_u16(), 200);
        let headers = response.headers().clone();
        assert_eq!(headers.get("accept-ranges").unwrap(), "bytes");
        assert_eq!(headers.get("content-type").unwrap(), "video/mp4");
        assert!(headers.contains_key("etag"));
        assert!(headers.contains_key("last-modified"));
        assert_eq!(
            headers.get("content-length").unwrap().to_str().unwrap(),
            BODY.len().to_string()
        );
        assert_eq!(response.bytes().await.unwrap().as_ref(), BODY);
    })
    .await;
}

#[tokio::test]
async fn a_range_request_returns_206_and_the_right_bytes() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        rig.write_download("clip.mp4", BODY);

        for (header, expected, content_range) in [
            ("bytes=0-9", &BODY[0..10], "bytes 0-9/30"),
            ("bytes=10-19", &BODY[10..20], "bytes 10-19/30"),
            ("bytes=20-", &BODY[20..], "bytes 20-29/30"),
            ("bytes=-5", &BODY[25..], "bytes 25-29/30"),
        ] {
            let response = rig
                .http
                .get(rig.url("download/clip.mp4"))
                .header("range", header)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 206, "{header}");
            assert_eq!(
                response.headers().get("content-range").unwrap(),
                content_range,
                "{header}"
            );
            assert_eq!(
                response.bytes().await.unwrap().as_ref(),
                expected,
                "{header}"
            );
        }

        // Unsatisfiable: 416 with the total.
        let response = rig
            .http
            .get(rig.url("download/clip.mp4"))
            .header("range", "bytes=900-999")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 416);
        assert_eq!(
            response.headers().get("content-range").unwrap(),
            "bytes */30"
        );
    })
    .await;
}

#[tokio::test]
async fn if_range_and_if_none_match_both_work() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        rig.write_download("clip.mp4", BODY);
        let response = rig.get_raw("download/clip.mp4").await;
        let etag = response
            .headers()
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();

        // A matching `If-Range` keeps the range.
        let response = rig
            .http
            .get(rig.url("download/clip.mp4"))
            .header("range", "bytes=0-4")
            .header("if-range", &etag)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 206);
        assert_eq!(response.bytes().await.unwrap().as_ref(), &BODY[0..5]);

        // A stale `If-Range` means "send me the whole thing", never a range of a file that has
        // since changed.
        let response = rig
            .http
            .get(rig.url("download/clip.mp4"))
            .header("range", "bytes=0-4")
            .header("if-range", "\"0-0\"")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(response.bytes().await.unwrap().len(), BODY.len());

        // `If-None-Match` is a 304 with no body.
        let response = rig
            .http
            .get(rig.url("download/clip.mp4"))
            .header("if-none-match", &etag)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 304);
        assert!(response.bytes().await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn the_audio_route_serves_the_audio_root() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        std::fs::write(rig.cfg.paths.audio_download.join("song.m4a"), BODY).unwrap();
        let response = rig.get_raw("audio_download/song.m4a").await;
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(response.bytes().await.unwrap().as_ref(), BODY);

        // The two roots are separate: a video path is not reachable through the audio route.
        rig.write_download("clip.mp4", BODY);
        let (status, _) = rig.get("audio_download/clip.mp4").await;
        assert_eq!(status, 404);
    })
    .await;
}

#[tokio::test]
async fn traversal_and_symlink_escapes_are_both_404() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let secret = rig.dir.path().join("outside");
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("secret.txt"), b"do not serve me").unwrap();

        // A component-wise containment check, so the legacy `startswith` bug cannot come back.
        // The encoded forms are the ones that actually reach the handler — a literal `..` is
        // normalised away by any conforming client before the request is sent — so they are the
        // ones asserted to carry the error envelope.
        // A missing file is the handler's own 404, with the envelope.
        let (status, body) = rig.get("download/nope.mp4").await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"], "not_found");

        // Every traversal spelling is a 404 too. The encoded forms are rejected by the router
        // before the handler sees them and the literal one is normalised away by the client, so
        // only the status is asserted here — what matters is that no spelling serves the file.
        for path in [
            "download/../outside/secret.txt",
            "download/%2e%2e/outside/secret.txt",
            "download/..%2Foutside%2Fsecret.txt",
        ] {
            let (status, body) = rig.get(path).await;
            assert_eq!(status, 404, "{path} answered {body}");
            assert!(
                !body.to_string().contains("do not serve me"),
                "{path} leaked the file"
            );
        }

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&secret, rig.download_dir().join("link")).unwrap();
            let (status, body) = rig.get("download/link/secret.txt").await;
            assert_eq!(status, 404, "a symlink must not escape the root: {body}");
        }
    })
    .await;
}

#[tokio::test]
async fn the_json_listing_appears_only_when_it_is_enabled() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        rig.write_download("Music/song.mp4", BODY);
        let (status, body) = rig.get("download/Music").await;
        assert_eq!(
            status, 404,
            "a directory is not browsable by default: {body}"
        );

        let indexable = Rig::builder(prefix)
            .env("DOWNLOAD_DIRS_INDEXABLE", "true")
            .start()
            .await;
        indexable.write_download("Music/song.mp4", BODY);
        indexable.write_download("Music/Live/other.mp4", BODY);
        let (status, body) = indexable.get("download/Music").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["path"], "Music");
        let files = body["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["name"], "song.mp4");
        assert_eq!(files[0]["size"], BODY.len());
        assert_eq!(files[0]["download_url"], "download/Music/song.mp4");
        let dirs = body["dirs"].as_array().unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0]["path"], "Music/Live");
    })
    .await;
}

#[tokio::test]
async fn a_finished_items_download_url_actually_resolves() {
    for_each_prefix(|prefix| async move {
        // The end-to-end shape a client uses: add, wait, read `download_url`, open it.
        let rig = Rig::start(prefix).await;
        let id = rig.add("https://fake.test/clip-one").await;
        let item = rig.until_status(&id, "finished").await;
        let url = item["download_url"].as_str().unwrap().to_owned();
        assert!(!url.starts_with("http"), "relative to <p>: {url}");
        // The `fake` provider names the file but does not write it, so the bytes are put in place
        // here — the point of the assertion is that the URL the item advertises is the URL the
        // file route accepts.
        rig.write_download(item["filename"].as_str().unwrap(), BODY);
        let response = rig.get_raw(&url).await;
        assert_eq!(response.status().as_u16(), 200, "{url}");
        assert_eq!(response.bytes().await.unwrap().as_ref(), BODY);
    })
    .await;
}

// ---------------------------------------------------------------------------
// what the route must never serve
// ---------------------------------------------------------------------------

/// The shipped image sets `DOWNLOAD_DIR=/downloads STATE_DIR=/downloads/.metube`, so STATE_DIR is
/// *inside* the served root and containment cannot exclude it. `cookies.txt` is the operator's
/// live site sessions and `aulos.db` is the whole queue, history and Telegram config; neither is
/// reachable through a route whose stock posture has no auth at all (DESIGN §16.6).
#[tokio::test]
async fn the_state_directory_is_never_served_even_when_it_is_inside_the_download_root() {
    for_each_prefix(|prefix| async move {
        // The shipped layout, in shape: `STATE_DIR` (and the database) inside `DOWNLOAD_DIR`
        // — `docker/Dockerfile` sets `DOWNLOAD_DIR=/downloads STATE_DIR=/downloads/.metube`. It
        // is nested one level deeper here only so the enclosing directory can be listed, since the
        // `download/{*path}` route has no spelling for the root itself.
        let rig = Rig::builder(prefix)
            .env("DOWNLOAD_DIRS_INDEXABLE", "true")
            .env("STATE_DIR", "{dir}/downloads/Media/.metube")
            .env("AULOS_DB_PATH", "{dir}/downloads/Media/.metube/aulos.db")
            .start()
            .await;
        let state_dir = rig.cfg.paths.state.clone();
        assert!(
            state_dir.starts_with(rig.download_dir()),
            "the test must reproduce the shipped layout: {state_dir:?}"
        );
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("cookies.txt"),
            b"# Netscape HTTP Cookie File\nSECRET",
        )
        .unwrap();
        std::fs::write(state_dir.join("aulos.db"), b"SQLite format 3\0").unwrap();
        rig.write_download("Media/ok.mp4", BODY);

        for path in [
            "download/Media/.metube/cookies.txt",
            "download/Media/.metube/aulos.db",
            "download/Media/.metube",
        ] {
            let response = rig.get_raw(path).await;
            assert_eq!(response.status().as_u16(), 404, "{path}");
            let text = response.text().await.unwrap();
            assert!(!text.contains("SECRET"), "{path} leaked the cookie jar");
            assert!(!text.contains("SQLite"), "{path} leaked the database");
        }

        // Nor is it advertised in the listing a prober would read first.
        let (status, body) = rig.get("download/Media").await;
        assert_eq!(status, 200, "{body}");
        let names: Vec<&str> = body["dirs"]
            .as_array()
            .unwrap()
            .iter()
            .chain(body["files"].as_array().unwrap())
            .filter_map(|e| e["name"].as_str())
            .collect();
        assert!(names.contains(&"ok.mp4"), "{names:?}");
        assert!(!names.contains(&".metube"), "{names:?}");
    })
    .await;
}

/// The download tree shares an origin with the API and the WebSocket, and in the intended VPS
/// deployment that origin carries the reverse proxy's session cookie. An `*.html` or `*.svg` that
/// lands in the tree — through the share the volume is exported over, or a `command` plugin — must
/// not execute script there.
#[tokio::test]
async fn a_scriptable_file_is_served_as_an_opaque_attachment() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        rig.write_download(
            "evil.html",
            b"<script>fetch('/api/v2/items/clear')</script>",
        );
        rig.write_download("evil.svg", b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>");
        rig.write_download("notes.txt", b"hello");

        for name in ["evil.html", "evil.svg", "notes.txt"] {
            let response = rig.get_raw(&format!("download/{name}")).await;
            assert_eq!(response.status().as_u16(), 200, "{name}");
            let headers = response.headers().clone();
            assert_eq!(
                headers.get("x-content-type-options").unwrap(),
                "nosniff",
                "{name}"
            );
            assert_eq!(
                headers.get("content-type").unwrap(),
                "application/octet-stream",
                "{name}"
            );
            let disposition = headers
                .get("content-disposition")
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            assert!(
                disposition.starts_with("attachment;"),
                "{name}: {disposition}"
            );
        }

        // Media keeps its real type and stays inline, so playback and `Range` are unaffected.
        rig.write_download("clip.mp4", BODY);
        let response = rig.get_raw("download/clip.mp4").await;
        assert_eq!(response.headers().get("content-type").unwrap(), "video/mp4");
        assert_eq!(
            response.headers().get("x-content-type-options").unwrap(),
            "nosniff"
        );
        assert!(response.headers().get("content-disposition").is_none());
    })
    .await;
}

/// RFC 9110 §14.2: a `Range` header the server cannot parse, or whose unit it does not know, is
/// **ignored** — `416` is reserved for a valid-but-unsatisfiable byte-range-set. A multi-range set
/// is legal, and a downloader that sends one must not be told the file is unfetchable.
#[tokio::test]
async fn an_unparseable_or_multi_range_header_serves_the_whole_file() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        rig.write_download("clip.mp4", BODY);

        for header in [
            "bytes=0-9,20-29",
            "bytes=abc",
            "bytes=-",
            "items=0-1",
            "bytes",
        ] {
            let response = rig
                .http
                .get(rig.url("download/clip.mp4"))
                .header("range", header)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 200, "{header}");
            assert_eq!(response.bytes().await.unwrap().as_ref(), BODY, "{header}");
        }

        // A satisfiable one still ranges, and an unsatisfiable one is still a 416.
        for (header, status) in [("bytes=0-4", 206), ("bytes=900-999", 416)] {
            let response = rig
                .http
                .get(rig.url("download/clip.mp4"))
                .header("range", header)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), status, "{header}");
        }
    })
    .await;
}
