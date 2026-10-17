//! The page service (spec/design/page-service.md): one owner process serves pages to client
//! processes over a local endpoint.
//!
//! Phase 1 is the wire codec ([`codec`]): framing, the message catalog, and the owner record,
//! hand-written from spec/pageservice/ and tested against its shared byte fixtures. Phase 2 is the
//! read path under the test-only `Locking::Service` (spec/pageservice/README.md §6): the election at
//! open ([`election`]), the serving owner ([`owner`]), the page client ([`client`]), and the
//! endpoint and owner record on the filesystem ([`endpoint`]). Phase 3 is the write path: client
//! write transactions under the owner's writer gate, and the owner's validation of their
//! CommitDeltas ([`delta`]). Unix only: Windows serving is not yet specified (page-service.md
//! §14), so `Locking::Service` fails `0A000` elsewhere.

#[cfg(unix)]
pub(crate) mod client;
pub mod codec;
#[cfg(unix)]
pub(crate) mod delta;
#[cfg(unix)]
pub(crate) mod election;
#[cfg(unix)]
pub(crate) mod endpoint;
#[cfg(unix)]
pub(crate) mod owner;
