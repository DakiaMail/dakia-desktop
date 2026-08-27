# Release source provenance

Production macOS releases are built locally from a clean `main` commit that
exactly matches `origin/main`. Beginning with v0.4.1, the release path requires
an annotated SSH-signed tag whose local and remote tag objects both target that
exact commit before a GitHub draft or R2 object can be published.

## Transitional v0.4.0 baseline

The public v0.4.0 updater artifacts were built and published from commit
`d336f6fbda0e78bad2f92e89b04998dc441cc7cb`. No v0.4.0 source tag was created.
The equivalent release-preparation change later entered `main` as
`4a5450e8253b8f36d5c97bbceacf2ac219737ddc`, but that later tree includes other
changes and is not the production artifact baseline. Release eligibility and
notes for v0.4.1 therefore use `d336f6f..main`.
