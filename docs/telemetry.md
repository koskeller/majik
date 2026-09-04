# Telemetry in Majik

Majik collects anonymous usage data and crash reports to see which features get used and why the
app crashes. Both are on by default and both can be turned off, together or separately, on the
first-launch screen or later in **Settings → Telemetry**.

Nothing you make is ever sent: not a prompt, not an image, not a file name, not the path of your
library, not an API key. Every event carries only *what kind* of thing happened.

## The two switches

- **Crash reports** (`diagnostics`). When Majik crashes, a second process it keeps beside itself
  writes a [minidump](https://learn.microsoft.com/en-us/windows/win32/debug/minidump-files) and a
  short report next to the app's log. The next launch uploads the pair and deletes it. A minidump
  is a snapshot of the crashed process's stacks and registers, so it can contain fragments of
  whatever was in memory at the time. The report beside it is the `CrashInfo` struct in
  [`crates/majik-crashes/src/lib.rs`](../crates/majik-crashes/src/lib.rs): the app version and
  commit, the release channel, the panic message and its source location if there was one, the
  GPU, and on Linux the C runtime's abort message.
- **Usage data** (`metrics`). Events like "Generation Requested", with the provider, the model
  name, the media type and the batch size. The full list is below.

With crash reports off, the report is still written to disk (it is yours to send by hand) but
never uploaded. With usage data off, events are dropped before they are queued; the one exception
is the "Telemetry Toggled" event that records the switch itself, sent as the last event when you
turn usage data off and as the first when you turn it on, so a build that ignored the switch would
show up in the data.

## Where it goes, and when

Events queue in memory and are posted to `https://trymajik.com/api/telemetry/events` every five
minutes, or sooner once fifty have accumulated, and once more when the app quits. Crash reports go
to `https://trymajik.com/api/telemetry/crashes` on the launch after the crash. Every request is
signed with a checksum only Majik's own release builds can produce, so the server can tell them
from anything else. There are no third parties: the server is ours.

A batch carries, besides the events:

| Field | What it is |
| --- | --- |
| `installation_id` | A random id minted the first time this install runs, in `config.json`. Not tied to you, your machine or your account (there is none). |
| `session_id` | A random id per launch. |
| `app_version`, `release_channel` | Which build. |
| `os_name`, `os_version`, `architecture` | `macOS 15.6.1`, `Windows 10.0.26100`, `Linux Wayland ubuntu 24.04`, and the CPU architecture. |

Development builds (`cargo run`) send nothing unless pointed at a server with
`MAJIK_TELEMETRY_URL`.

## Reading what was sent

**Help → View Telemetry Log** (or Settings → Telemetry) lists every event this app has queued,
newest first, with its properties. The same events are appended to `telemetry.log` in the logs
folder (**Settings → About → Show Logs**), one JSON line each, exactly as they were sent. The log
is truncated at every launch.

## The events

| Event | Properties |
| --- | --- |
| App First Opened / App Opened | — |
| App Closed | `session_seconds` |
| Onboarding Completed / Onboarding Skipped | `provider` |
| Telemetry Toggled | `setting` (`metrics` / `diagnostics`), `enabled` |
| Generation Requested | `provider`, `model`, `media_type`, `tool`, `input_count`, `batch` |
| Generation Finished | `provider`, `model`, `media_type`, `tool`, `outcome` (`completed` / `failed` / `cancelled`), `error_kind` (the error's category, never its message), `attempt`, `duration_ms` |
| Generation Retried | `count` |
| Prompt Improved | `provider`, `media_type` |
| Files Imported | `count`, `failed` |
| Media Saved / Media Copied / Media Dragged | `count` |
| Album Created | — |
| Layout Changed | `layout` |
| Settings Changed | `setting` (`appearance`, `reduce_motion`, `library_root`), `value` for the first two |
| Provider Key Added / Provider Key Removed | `provider` |
| Minidump Uploaded | `panic_message`, `crashed_version`, `commit_sha` |

Every event also carries `event_source: "majik"`.

## Reading a crash report yourself

A report is `<session>.dmp` (zstd-compressed) and `<session>.json` in the logs folder. To read
the dump you need the symbols of the exact build, which the release attaches beside the
installers as `majik-<target>.sym` (the report's `app_version` and `commit_sha` say which
release):

```sh
zstd -d <session>.dmp -o minidump.dmp
mkdir -p symbols && cp majik-<target>.sym symbols/
minidump-stackwalk --symbols-path symbols minidump.dmp
```

`minidump-stackwalk` is `cargo install minidump-stackwalk`.

## Questions

Email hello@trymajik.com or open an issue.
