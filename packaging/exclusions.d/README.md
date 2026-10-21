# Sensitive-tree exclusion templates

These are not drop-in files the daemon reads automatically — rfindd's
config schema is a single `scan.exclude` array in
`/etc/rfindd/config.toml`. Each `.toml` here is a *reference fragment*
you can paste into that array. See `docs/privacy.md` for context.

A real `scan.exclude` would concatenate the lists from the templates
you care about. The order inside the array doesn't matter.

Templates assume a single system user `alice`. For multi-user hosts,
duplicate each `/home/alice/...` line for every account, or exclude
`/home` wholesale and add back only the subtrees you want searchable
via `scan.roots`.
