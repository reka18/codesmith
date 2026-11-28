# Test data from the CodeSmith Mobile app

These are the app team's fixtures from `transport/fixtures/`, copied byte
for byte and pinned (remote-requirements S24):

| File | App repo commit | sha256 |
|---|---|---|
| `pairing.json` | `ba23710` (main) | `c4708104b524a984285bf46241c3f03673f9fbe3759f028735d4c9181c642e0c` |
| `remote_events.json` | `d6396fd` (main) | `e71a2eafc31743ca09de2f6d8a448800d3d8299e27f2a655650d241b64f69fcc` |

Don't edit them here. When the app team changes one, copy the new file and
update the commit and hash in this table. The `remote_events.json` test
(`remote/fixture_tests.rs`) checks the file's hash against this table.
