"""Stub of ``yt_dlp.networking.impersonate``, for the ``coerce`` mechanism of DESIGN §9.2."""


class ImpersonateTarget:
    """The object a coerced ``impersonate`` option string becomes."""

    def __init__(self, client):
        self.client = client

    @classmethod
    def from_str(cls, value):
        """Parses ``chrome``, ``chrome-120`` and friends. Rejects the empty string."""
        if not value:
            raise ValueError("invalid impersonate target")
        return cls(value)

    def __repr__(self):
        return f"ImpersonateTarget({self.client!r})"
