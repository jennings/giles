# giles-channel-whatsapp

The WhatsApp channel plugin for Giles. See the user guide (`Docs/user/whatsapp.md`) and the
technical page (`Docs/technical/whatsapp.md`).

## License

This crate is licensed under the GNU Affero General Public License, version 3 only
(`AGPL-3.0-only`, see `LICENSE-AGPL-3.0`), not under the `MIT OR Apache-2.0` that the rest of
Giles uses.

The plugin links `whatsapp-rust`, which implements the Signal protocol with `wacore-libsignal`, a
port of Signal's `libsignal`. Its source files carry `SPDX-License-Identifier: AGPL-3.0-only`
(Copyright Signal Messenger, LLC), although the crate's manifest says `MIT`. A binary that
includes that code is a combined work under the AGPL, so the plugin binary is distributed under
it. `THIRD-PARTY` lists the components and where their source is.

The Giles core and every other crate stay `MIT OR Apache-2.0`. The plugin is a separate process
that talks to the core over gRPC (`giles-plugin-sdk`), and nothing in the core links it. This is a
description of how the code is licensed, not legal advice.
