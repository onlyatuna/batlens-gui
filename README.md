# batlens-gui

Windows laptop battery tray tool. Uses [batlens-core](https://github.com/onlyatuna/batlens-core) for battery data.

- **Left click**: toggles a frameless popup panel above the tray icon with charge percent, state, progress bar, and two-column rows for power, health, wear rate, time left, and capacity.
- **Right click**: settings menu (`Icon shows` -> `Charge` / `Health`, `Start with Windows`, `Quit`).
- **Updates**: listens for `WM_POWERBROADCAST` (`GUID_ACDC_POWER_SOURCE` / `PBT_APMPOWERSTATUSCHANGE`) for immediate AC/charging transitions plus a 30-second poll; computes wear rate on a background thread from the battery report cache.
- **Persistence**: `HKCU\Software\batlens-gui\IconMode` (`REG_DWORD`) and `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\batlens-gui` (`REG_SZ`).

## Build

```
cargo build --release
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT, see [LICENSE](LICENSE).
