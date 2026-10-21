# Record reader differential diagnostic

Status: research. The in-house reader's goal is to publish source records into the database.
Legacy-reader parity is not an acceptance objective, correctness oracle, or row-equality gate.

`reader-diff` is a bounded diagnostic for differences between two existing world databases. It
compares shared tables and canonical records, but does not verify that the databases came from the
same source inventory, plugin order, reader revision, world schema, language, or localized string
banks. It does not establish why a difference exists or whether either database is complete.
Differences are observations to investigate; they are not automatically bugs or evidence of a
specific causal mechanism. A real-data run can characterize those selected inputs only.

## Comparison behavior

- Shared table rows are compared by their declared primary key. Shared projected columns are
  compared directly, including the `data` blobs in `cells`, `references`, and `waters`.
- `records.data` is decoded as the canonical record archive and compared by subrecord occurrence.
  A corrupt archive fails the comparison with its database path and FormID. Invalid bytes never
  become an empty record.
- The field comparison does not reliably associate each compared occurrence with schema type or
  localization provenance. It reports differing lengths as `other_length` and same-length values
  as `other_bytes`. It does not infer localized strings, cleared links, FormID remaps, or text
  encoding from byte shape alone.
- A difference is not normalized away. The report does not claim equal source inputs or explain
  reader behavior unless a separate, typed evidence source supports that conclusion.

The fixture originated against PR #280's pinned historical base `27d48513429a883903abed0e4a98a3700b12dde8`.
It contains eight input cases across seven behaviors from that snapshot: two malformed fields, one
out-of-order field, one dangling reference, one typed link, one Windows-1252 string, one localized
string, and one rejected override. Later main changes now align the typed `WTHR IMSP` remap and
localized `ACTI FULL` text in both readers; the fixture checks those outputs have converged. These
cases characterize the selected synthetic inputs. They do not establish real-data completeness
or validate the comparator as an oracle.

## Run the diagnostic

Use the workspace wrapper for Cargo. Keep generated databases under shared `scratch/` and retained
reports under shared `evidence/`; choose an owner-specific task name. Replace these quoted paths
with absolute paths on the host. The angle-bracket characters are explanatory prose only and must
not appear in shell commands.

```sh
storage="/absolute/path/to/.mudcrab-local"
data="/absolute/path/to/Skyrim/Data"
plugins="/absolute/path/to/plugins.txt"
out="$storage/scratch/reader-diff-pr280"
report="$storage/evidence/reader-diff-pr280.json"

python3 scripts/workspace.py cargo --isolated reader-diff-pr280 run --locked --release \
  -p converter --bin reader-diff -- \
  build "$data" "$plugins" "$out" --json "$report"
```

To compare existing databases, pass their absolute paths. This mode still does not verify their
input identity:

```sh
legacy_db="/absolute/path/to/legacy/skyrim_world.db"
inhouse_db="/absolute/path/to/inhouse/skyrim_world.db"

python3 scripts/workspace.py cargo --isolated reader-diff-pr280 run --locked --release \
  -p converter --bin reader-diff -- \
  compare "$legacy_db" "$inhouse_db" --json "$report"
```

The JSON and text output summarize observed differences and bounded samples. They do not rank
records by importance, prove that either reader omitted a source record, or qualify native runtime,
performance, rendering, or visual parity. A real-data run requires separately recorded source
inventory, plugin order, reader and schema revisions, language/string-bank selection, and output
provenance before making dataset-specific claims.
