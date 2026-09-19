# Manuvra

Manuvra is being rebuilt as a Jev-driven browser flow executor in Rust. It will accept structured jobs, control a dedicated Chromium instance over the Chrome DevTools Protocol, and retain evidence for each run while using typed model judgments to select browser operations.

Linux is the first supported development and execution platform. macOS support will follow after the new executor architecture is established.

## Development

```bash
make fmt
make lint
make test
make crap
```

The current foundation and design research is in [`docs/research`](docs/research/2026-09-19-jev-browser-executor-foundations.md). A design brief for the restarted executor is forthcoming.

## License

MIT. See [LICENSE](LICENSE).
