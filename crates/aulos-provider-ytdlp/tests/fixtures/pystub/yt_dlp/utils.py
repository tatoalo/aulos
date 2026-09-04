"""The exception classes ``ytdlp_runner.classify`` looks up by name (DESIGN §9.6)."""


class YoutubeDLError(Exception):
    """Base class, as upstream."""


class DownloadError(YoutubeDLError):
    """Wraps a transport or extraction failure. ``exc_info`` is the wrapped triple."""

    def __init__(self, msg, exc_info=None):
        super().__init__(msg)
        self.exc_info = exc_info


class ExtractorError(YoutubeDLError):
    """An extractor-level failure."""

    def __init__(self, msg, ie=None, expected=False):
        super().__init__(msg)
        self.ie = ie
        self.expected = expected


class UnsupportedError(ExtractorError):
    """No extractor claimed the URL."""


class GeoRestrictedError(ExtractorError):
    """Blocked in this region."""


class PostProcessingError(YoutubeDLError):
    """A postprocessor failed."""
