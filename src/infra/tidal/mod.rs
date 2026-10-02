//! Tidal media source.
//!
//! Browses the user's Tidal library through the private client API
//! (`api.tidal.com/v1`, the one the python-tidal ecosystem uses) and plays
//! tracks through the shared [`LocalPlayer`](crate::infra::audio::LocalPlayer).
//! The official developer API serves third-party clients 30-second previews
//! only.
//!
//! ## URIs
//!
//! Tracks: `tidal:track:<id>`. Sidebar rows (each opens the shared track table):
//! `tidal:favorites:tracks`, `tidal:playlist:<uuid>`, `tidal:album:<id>`.
