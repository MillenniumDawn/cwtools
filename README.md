# cwtools
A library for parsing, editing, and validating Paradox Interactive script files.

> **Fork notice:** This is a fork of [cwtools/cwtools](https://github.com/cwtools/cwtools). The original F# library (NuGet packages, .NET Standard) lives at the upstream repo. Please give them their love as well for inspiring this wonderful project.

> **Game support:** Right now we predominantly support **Hearts of Iron IV**. The validator is built in Rust (see `cwtools-rs/`) and HOI4 is where it's complete and tested. **Stellaris** also ships native validators (CW108/109/110/120/227/229/250 plus the if/else and set_name checks CW236/237/238/253). The other games (EU4, CK2/CK3, Vic2/Vic3, Imperator) parse, but their validation and per-game rules are partial while we get the foundation right. Full multi-game parity is tracked in the [issues](https://github.com/MillenniumDawn/cwtools/issues).

## Documentation

- [Architecture](cwtools-rs/docs/ARCHITECTURE.md) — the crate map, the batch pipeline, the CLI-vs-LSP split, and LSP features like the idle-gated background reindex.
- [CWXXX error/warning code reference](cwtools-rs/docs/ERROR_CODES.md) — full catalog of diagnostic codes emitted by the Rust validator.
- [Profiling guide](cwtools-rs/PROFILING.md) — how to measure validation performance.

## Projects that use CW Tools
#### [Stellaris tech tree](http://www.draconas.co.uk/stellaristech): https://github.com/draconas1/stellaris-tech-tree
An interactive tech tree visualiser that uses CW Tools to parse the vanilla tech files, and extract localisation.
#### [SC Mod Manager](https://github.com/WojciechKrysiak/SCModManager): https://github.com/WojciechKrysiak/SCModManager/tree/feature/PortToAvalonia/PDXModLib/Utility
A mod manager that uses CW Tools for parsing and manipulating mod files.

