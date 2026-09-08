//! Library surface for `tfls-cli`, so pure logic used by the binaries
//! (`tfls-lint`'s output renderers, in particular) can be unit-tested
//! without going through a subprocess.

pub mod lint_output;
