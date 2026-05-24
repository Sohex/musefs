mod art;
mod error;
mod models;
mod schema;
mod tags;
mod tracks;

pub use error::{DbError, Result};
pub use models::{Art, ArtMeta, Format, NewArt, NewTrack, Tag, Track, TrackArt};

use rusqlite::Connection;
use std::path::Path;

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Db> {
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        schema::migrate(&mut conn)?;
        Ok(Db { conn })
    }

    pub fn open_in_memory() -> Result<Db> {
        let mut conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", true)?;
        schema::migrate(&mut conn)?;
        Ok(Db { conn })
    }

    pub fn user_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?)
    }

    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "data_version", |r| r.get(0))?)
    }
}
