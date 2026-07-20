//! The `java --version` banner string.

/// The Java language level javars targets — reported by `java --version` so
/// build tools that parse the version line read a familiar level, followed by
/// the real engine so nothing is misrepresented as the JDK.
pub const JAVA_COMPAT_VERSION: &str = "21";

/// The engine name — javars is its own runtime (like the JVM is HotSpot).
pub const JAVA_ENGINE: &str = "javars";

/// The `RUNTIME_PLATFORM` string, built from the host arch/OS.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// The `java --version` banner. Names the targeted language level, then the
/// real engine and its crate version and host triple.
pub fn version_banner() -> String {
    format!(
        "java {} (javars {}) [{}]",
        JAVA_COMPAT_VERSION,
        env!("CARGO_PKG_VERSION"),
        platform()
    )
}
