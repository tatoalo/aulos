import json
import os
from pathlib import Path
from urllib.parse import urlsplit

from yt_dlp.extractor.common import InfoExtractor


class AulosSingleVideoIE(InfoExtractor):
    _VALID_URL = r"https://single\.test/(?P<kind>watch|video|playlist)/(?P<id>[^/?#]+)"

    def _real_extract(self, url):
        match = self._match_valid_url(url)
        kind, video_id = match.group("kind", "id")
        record = {
            "url": url,
            "flat": self.get_param("extract_flat", False),
            "noplaylist": self.get_param("noplaylist", False),
        }
        with Path(os.environ["TEST_SINGLE_VIDEO_LOG"]).open("a") as log:
            log.write(json.dumps(record) + "\n")
        if kind == "playlist" or (
            kind == "watch" and urlsplit(url).query and not record["noplaylist"]
        ):
            return self.playlist_result(
                [
                    self.url_result(
                        f"https://single.test/video/{video_id}-{suffix}",
                        self.ie_key(),
                        video_id=f"{video_id}-{suffix}",
                        video_title=f"{video_id}-{suffix}",
                    )
                    for suffix in ("one", "two")
                ],
                video_id,
                "Playlist",
            )
        if kind == "watch":
            return self.url_result(f"https://single.test/video/{video_id}", self.ie_key())
        return {
            "id": video_id,
            "title": video_id,
            "webpage_url": url,
            "url": os.environ["TEST_SINGLE_VIDEO_MEDIA"],
            "ext": "mp4",
        }
