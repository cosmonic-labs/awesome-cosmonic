# Workload examples

Small, complete applications you can run in Cosmonic Desktop. Each one builds
to a single component, ships with the manifest that deploys it, and has a
published image on GHCR, so you can run it before you build it.

| Example | What it does | State | Outbound network |
| --- | --- | --- | --- |
| [`first-flight`](first-flight/) | The page behind the **First Flight** Launchpad entry. | None | None |
| [`qrcode`](qrcode/) | Turns text into a QR-code PNG. | None | None |
| [`csv-tidy`](csv-tidy/) | Cleans a messy CSV into JSON and lists what was wrong. | None | None |
| [`passphrase`](passphrase/) | Generates passwords and reports their entropy. | None | None |
| [`request-bin`](request-bin/) | Records every request sent to a disposable URL. | Key-value store | None |
| [`task-manager`](task-manager/) | A to-do list. | Key-value store | None |

One directory per hosted example. See [CONTRIBUTING.md](../CONTRIBUTING.md)
for requirements. In particular, model weights and large fixtures belong
behind a download script rather than in git history.

For starting points maintained by Cosmonic rather than the community, see the
[Template Catalog](https://cosmonic.com/docs/template-catalog/).
