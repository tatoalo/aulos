# Provider regression fixtures

These fixtures preserve the format selectors, yt-dlp option dictionaries, and progress behavior
used by current Aulos providers. They remain active after retirement of the MeTube HTTP API.

| Fixture | Consumer |
|---|---|
| `tests/golden/formats.json` | yt-dlp format selector tests |
| `tests/golden/opts.json` | yt-dlp option tests |
| `tests/golden/percent.json` | progress normalizer tests |

To regenerate against a legacy source checkout with its Python dependencies installed:

```sh
METUBE_POT_ROOT=/path/to/metube_pot PYTHON=/path/to/metube_pot/.venv/bin/python tools/capture/run_capture.sh
```

The script runs `dump_formats.py`, `dump_progress_vectors.py`, then `verify.py`. `_legacy.py` supplies source-loading and canonical JSON helpers.

CI validates fixture shape, selector coverage, option invariants and progress vectors offline:

```sh
python3 tools/capture/verify.py
```
