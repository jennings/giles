# Data Root

Everything Giles stores lives under one directory, the data root (`GILES_HOME`, PLAN section 11.1).
The module `giles_core::paths` owns its layout: no other code joins path strings onto the root.

## Resolution

The root is the first of:

1. the `--home <DIR>` flag,
2. the `GILES_HOME` environment variable, if set and non-empty,
3. `$HOME/.giles`.

If none applies (no flag, and both variables unset or empty), startup fails with an error naming
`GILES_HOME` and `HOME`. A relative root is made absolute against the current directory, and every
root is normalized lexically: repeated and trailing separators and `.` components are dropped. `..`
and symlinks are kept as written, since resolving them needs the filesystem.

`resolve_root` is a pure function of those three inputs; `DataRoot::from_env` feeds it the process
environment.

## Layout

```text
<root>/
  giles.toml          DataRoot::config_file()       human-written
  secrets.toml        DataRoot::secrets_file()      human-written, mode 0600
  agents/<agent>/     DataRoot::agent_dir(name)     agent.toml, SOUL.md, skills/, ...
    memory.db         DataRoot::agent_memory_db()   the agent's memories, created on first use
  users/<user>/       DataRoot::user_dir(name)      USER.md, the person-written profile
  skills/             DataRoot::skills_dir()
  plugins/            DataRoot::plugins_dir()       plugin binaries
  data/               DataRoot::data_dir()
    giles.db          DataRoot::database_file()
    blobs/            DataRoot::blobs_dir()
    plugins/          DataRoot::plugin_data_root()
      <instance>/     DataRoot::plugin_data_dir(name)
  backups/            DataRoot::backups_dir()
  trash/              DataRoot::trash_dir()         not in the layout: made at the first removal
    agents/           removed agents: <agent>-<UTC time>/
  run/                DataRoot::run_dir()           mode 0700
    admin.sock        DataRoot::admin_socket()      unless it falls back (below)
    admin.token       DataRoot::admin_token_file()
    giles.lock        DataRoot::lock_file()         flock held by the running core
    proxy.sock        DataRoot::proxy_socket()      unless it falls back (below)
    pids/             DataRoot::pids_dir()
      <instance>.pid  DataRoot::pid_file(name)
    sandbox/          DataRoot::sandbox_specs_dir()
      <instance>.json DataRoot::sandbox_spec_file(name)
  cache/              DataRoot::cache_dir()
```

Building a `DataRoot` does no I/O, so a CLI client computes the same paths, including the socket
paths, as the daemon serving that root.

Accessors that take an agent, user, or instance name return `PathsError::InvalidName` unless the
name is a single normal path component (`is_valid_name`: non-empty, no `/` or NUL, not `.` or
`..`), so a name can never escape its parent directory. This is a path-safety backstop, not the name
grammar: config validation applies the stricter `[a-zA-Z0-9-]+`, but names also arrive at
runtime (e.g. a user created through the admin service), and those must not be able to traverse
paths either. It matters most for `data/plugins/<instance>/`, the only directory a plugin can write.

## Creating the Layout

`DataRoot::create_layout` runs at `giles serve` startup and is idempotent. It creates the root and
every directory above, each with mode 0700. It creates no files: `giles.toml` and `secrets.toml` are
written by people, and the database, token, sockets, and pid files by the subsystems that own them.

Existing directories keep their modes, so an operator may loosen one deliberately, except `run/`,
which is reset to 0700 on every start because it holds the admin token and the sockets.

## Socket Path Fallback

A Unix socket path must fit in `sockaddr_un.sun_path` with its terminating NUL: 104 bytes on macOS
and the BSDs, 108 on Linux, so at most 103 and 107 bytes of path (`MAX_SOCKET_PATH_LEN`). A test
binds sockets at exactly the limit and one byte past it to confirm the constant against the
platform.

When `<root>/run/admin.sock` or `<root>/run/proxy.sock` would be longer than that, both sockets move
to a private directory under the temporary directory (`$TMPDIR`, else `/tmp`; an empty `TMPDIR`
counts as unset):

```text
$TMPDIR/giles-<16 hex digits>/admin.sock
$TMPDIR/giles-<16 hex digits>/proxy.sock
```

- The name is the 64-bit FNV-1a hash of the absolute, normalized root path. It is a fixed algorithm
  (unlike Rust's `DefaultHasher`), so the daemon and any `giles` CLI, even from another release,
  derive the same directory from the same `GILES_HOME` without coordinating.
- Only the sockets move. `run/admin.token`, `run/giles.lock`, and `run/pids/` stay in the data
  root.
- The temporary directory may be shared with other users (`/tmp`), and the name is predictable, so
  `create_layout` creates the directory with mode 0700 and, if it already exists, accepts it only if
  it is a real directory (not a symlink) owned by the owner of `run/`. It then resets its mode to
  0700.
- Clients must call `DataRoot::check_socket_dir` before connecting to `admin_socket()` or
  `proxy_socket()`. For the fallback it applies the same checks and also requires that no other user
  can access the directory. Otherwise another user could plant `admin.sock` there and receive the
  admin token. For `run/` it checks nothing.
- If the fallback path is also too long, startup fails and asks for a shorter `GILES_HOME` or
  `TMPDIR`.
- Spellings that differ only in repeated or trailing separators or `.` components are the same root
  (`/a/b`, `/a/b/`, `/a//b`). Spellings that differ through a symlink or `..` hash differently,
  because resolving those needs the filesystem. Use one spelling for the daemon and its CLI.

Plugins never get socket paths; the core hands them one end of a socket pair (PLAN section 7.4).
