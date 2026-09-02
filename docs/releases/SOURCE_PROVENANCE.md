# Release source provenance

Production releases are built by the GitHub-hosted `production-release`
workflow from a clean commit that exactly matches live `origin/main`. If the
workflow prepares a patch version, it pushes that commit first and every later
validation, build, publication, and verification job checks out that exact
pushed commit.

Before a GitHub Release draft or R2 object is published, the workflow requires
an annotated SSH-signed `vX.Y.Z` tag whose tag object and peeled commit are on
origin and whose commit exactly equals the release commit. It verifies an
existing tag instead of replacing it, and fails rather than producing an
unsigned source marker. The GitHub Release is a mirror of that tagged source
and becomes public only after R2 artifact and feed verification succeeds.

The manual Apple Silicon fallback follows the same provenance rule. It is not
permitted to build or publish from an arbitrary local checkout.

## Transitional v0.4.0 baseline

The public v0.4.0 updater artifacts were built and published from commit
`d336f6fbda0e78bad2f92e89b04998dc441cc7cb`. No v0.4.0 source tag was created.
The equivalent release-preparation change later entered `main` as
`4a5450e8253b8f36d5c97bbceacf2ac219737ddc`, but that later tree includes other
changes and is not the production artifact baseline. Release eligibility and
notes for v0.4.1 therefore use `d336f6f..main`.
