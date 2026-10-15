//! What the router does differently on Unix and on Windows, and nothing else.
//!
//! Both sides answer the same questions: where the router's door for local
//! nodes is, how a directory or file is kept to this user, how the election lock
//! is taken and who holds it, and which process serves the door a node dialled.
//! Everything above this module is one program.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;
