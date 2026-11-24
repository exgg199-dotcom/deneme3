# Horizon mods

The mod docs live in the modding book: [introduction](modding/src/introduction.md), [installing and using mods](modding/src/start/installing.md), [making a content mod](modding/src/how-to/content-mods.md) and [publishing](modding/src/how-to/publish.md).

On iOS and iPadOS, native mod code must be compiled into the signed app. The desktop launcher's game-rebuild workflow in the book does not run on the device. Java Fabric and NeoForge mods do not run unchanged in this mobile port.

Running Java mods unchanged needs a Java bridge, which is only ever a separate, optional download, never installed or enabled by default. The player decides whether to enable it. Promotion out of beta requires MSPT and memory benchmarks against the Java client with the same mods ([horizon-rust/horizon#13914](https://github.com/horizon-rust/horizon/issues/13914)).
