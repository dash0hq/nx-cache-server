# Local development

Keep the existing Rust/Axum/S3 separation. Conditional S3 creation is the only
collision authority. Never buffer a whole upload or add a HEAD-before-PUT check.
Use simple functions and early returns. Add no services or frameworks without a measured need.

Run `make check` first. `make test-s3` and `make test-nx` create disposable Docker
projects and need Docker Compose, Python 3, and Node 22/npm. Check resources before
those suites. Never aim fixtures at production or a developer's shared Nx cache.

All Cargo commands use the pinned toolchain and lockfile. `cargo fmt` formats Rust.
Read-only writes must drain within configured limits so Nx observes 403, not a broken pipe.
Preserve the 8 MiB raw-TCP regression tests. Logs must not contain tokens or artifact keys.
