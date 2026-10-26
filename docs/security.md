# Security model

Capricorn records your screen and clipboard. That history is about as sensitive as data
gets, so the design starts from one rule: nobody but you should be able to read it. That
includes other programs running under your own login.

## What is protected

| Attacker | What they get |
|---|---|
| Another program running as you (malicious package, compromised browser) while the vault is locked | Nothing from history. The files belong to a separate system user, and the key that opens them is only in memory while unlocked. |
| Someone with your disk or a stolen laptop | Ciphertext, plus which hours you were active (see "Metadata" below). |
| The network | Nothing. The daemon runs with no network access at all (`PrivateNetwork=yes`, AF_UNIX only). |
| Another user on the machine who is not in `services.capricorn.users` | They can't connect to the daemon's sockets. |

## What is not protected

- **Malware that is already running as you when you unlock.** It can capture the password
  (for example by registering as an input method or drawing a fake prompt) or attach to the
  unlocked UI session, and then read history the way the UI does. Malware that arrives
  while the vault is locked gets nothing until the next unlock. A hardware security key as
  a second factor is planned; the vault format already uses key slots for it.
- **Whatever is on screen right now.** While you are looking at your history, anything that
  can see your screen can see it too.
- **Root.** Root on a running machine can read memory or replace the daemon.
- **A forgotten password without a recovery code.** The history is unrecoverable. That is
  the point.

## How it works

### Processes

- `capricornd` runs as the system user `capricorn`. It owns `/var/lib/capricorn` (mode
  0700), does OCR, dedupe and secret redaction, encrypts every capture, and answers
  searches. systemd hardening: no network, no new privileges, read-only system, no home
  directories, no core dumps, a system-call filter and no capabilities.
- `capricorn-capture` runs as you, because it has to talk to your Wayland compositor. It
  holds no keys and writes nothing to disk. With the Home Manager module, Hyprland's
  permission system lets only this exact binary (matched by its full Nix store path) take
  screenshots; everything else gets Hyprland's default prompt.
- `capricorn-ui` and the `capricorn` CLI run as you. A session is tied to one socket
  connection: unlocking on one connection does not unlock it for any other. The UI marks
  itself non-dumpable so other processes running as you can't read its memory through
  `/proc`.

The daemon checks each client's uid (`SO_PEERCRED`) against the configured users, and the
sockets are only accessible to the `capricorn` group. It does not try to identify which
binary is connecting: it runs as a different uid, so it can't read the client's
`/proc/<pid>/exe`, and any program running as you could fake that anyway.

### Keys

- The vault has an X25519 key pair. The capture side only ever needs the public key, so
  recording keeps working while the vault is locked, and nothing that records can read
  what was recorded.
- The private key is wrapped by your password with Argon2id (tuned to roughly half a
  second to a second on your machine) and, optionally, by a recovery code.
- While unlocked, the private key exists only in the daemon's memory. Locking (timeout,
  `capricorn lock`, suspend, or a logind lock signal) drops it and the decrypted search
  index.

### Storage

- Captures are grouped by hour. During the current hour (plus a grace window so "forget
  the last few minutes" still works), each record is encrypted with XChaCha20-Poly1305
  under a random per-hour key. That key exists in daemon memory and, sealed to the vault's
  public key, in the file header.
- Once the hour closes, its records are rewritten into a single sealed file: one
  seekable XChaCha20-Poly1305 stream under a fresh key, sealed to the public key. Chunks
  are bound to their position and the final chunk is marked, so truncation and reordering
  are detected.
- The search index lives only in memory while unlocked and is saved as an encrypted
  snapshot.
- Text that looks like a secret (API tokens, private keys, JWTs and your own patterns) is
  never stored from the clipboard, and is blurred in screenshots and redacted from OCR
  text before anything is encrypted.

### Metadata

A disk thief can see which hours had activity and roughly how much was captured in each.
For an hour that has not been sealed yet (the current one, or one left open by a crash
until the next unlock) they can also see the number and size of individual captures.
Segment modification times are reset to the start of the hour, but change times
(`ctime`) and directory times still show roughly when the last capture was written, and
the index and key files show when the vault was last locked or its password changed.

## Host requirements

Plaintext passes through memory that can't be pinned: compositor buffers, the UI's
textures, OCR buffers. The NixOS module therefore refuses to build unless:

- every swap device is encrypted (random key or on LUKS),
- hibernation is off or resumes from an encrypted device,
- systemd-coredump stores nothing (`Storage=none`, set by default).

Full-disk encryption is strongly recommended and the module warns without it. Set
`services.capricorn.allowInsecureHost = true` to override these checks.

`kernel.yama.ptrace_scope` defaults to 1, so programs running as you can't attach to
processes they didn't start.

## Known gaps

- Password manager popups drawn inside a browser window (extension popups) aren't
  separate windows, so they can't be excluded by window rules. On-screen secret redaction
  catches some of what they show, not all.
- On rotated monitors, a frame that would need an excluded window blacked out is dropped
  entirely.
- Small leftovers of short-lived keys may remain in freed heap memory inside the
  encryption library until it is reused. The same goes for the decrypted search index
  and records after locking: the daemon drops them but does not overwrite every byte.
  With encrypted swap and no core dumps none of this reaches disk, and the daemon's
  memory is only readable by root.
