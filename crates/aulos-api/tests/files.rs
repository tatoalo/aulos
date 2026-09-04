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
