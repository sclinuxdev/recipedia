//! Recipedia -- sclinux recipe & repository hub.
//!
//! Read-only presentation of the sclinuxdev/recipes git tree plus built-in
//! hosting of published binary packages. The git repo is the single source of
//! truth for recipes; build state is never reported, it is derived on every
//! request as the diff between recipe versions and the `published` table.

pub mod config;
pub mod db;
pub mod model;
pub mod repo;
pub mod status;
pub mod sync;
pub mod web;
