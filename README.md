# bliss-guidance-lastfm

`bliss-guidance-lastfm` is a provider addon for the
`bliss-playlist-optimizer` guidance SPI. The initial implementation consumes
the existing `semantic-evidence-v1` raw artifact produced by Better Call Bliss.
It returns resolved, local-candidate Last.fm guidance for the current route
edge; unresolved provider identities are ignored.

Its SPI provider ID is `lastfm-guidance`. It does not contact Last.fm itself;
Better Call Bliss/LastMix remains responsible for obtaining and caching the
raw artifact.

This deliberately separates provider acquisition from optimizer scoring. A
future build may add direct anonymous Last.fm or LastMix transport without
changing the SPI.

Prepare options:

```json
{"artifact_path":"/path/to/semantic-evidence.json"}
```

The addon communicates through versioned JSONL on stdin/stdout. It contributes
bounded guidance only; Bliss acoustic quality and all hard route constraints
remain optimizer responsibilities.
