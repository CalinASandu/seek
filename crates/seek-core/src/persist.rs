//! On-disk index format: `MAGIC | version u32 | header_len u32 | header JSON | zstd(body)`.
//!
//! Body: one `u64 LE byte length + raw LE bytes` block per column (ids, parents, name_off, name_len,
//! lower_off, lower_len, sizes, modified, created, attrs), then the names and lower arenas, then the
//! index `root_id` as a bare `u64 LE`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::index::DeviceIndex;
use crate::model::{Checkpoint, DeviceId, HostId};
use crate::Error;

pub const INDEX_MAGIC: &[u8; 8] = b"SEEKIDX\0";
pub const INDEX_FORMAT_VERSION: u32 = 1;

const ZSTD_LEVEL: i32 = 3;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexHeader {
    pub format_version: u32,
    pub device_id: DeviceId,
    pub host_id: HostId,
    pub checkpoint: Option<Checkpoint>,
    /// Unix epoch milliseconds.
    pub saved_at: i64,
    pub rows: u64,
}

fn bad(msg: impl Into<String>) -> Error {
    Error::Format(msg.into())
}

trait Le: Copy {
    const SIZE: usize;
    fn put(self, out: &mut Vec<u8>);
    fn get(b: &[u8]) -> Self;
}

macro_rules! impl_le {
    ($($t:ty),*) => {$(
        impl Le for $t {
            const SIZE: usize = std::mem::size_of::<$t>();
            fn put(self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }
            fn get(b: &[u8]) -> Self {
                let mut a = [0u8; std::mem::size_of::<$t>()];
                a.copy_from_slice(b);
                <$t>::from_le_bytes(a)
            }
        }
    )*};
}
impl_le!(u16, u32, u64, i64);

fn put_col<T: Le>(out: &mut Vec<u8>, col: &[T]) {
    out.extend_from_slice(&((col.len() * T::SIZE) as u64).to_le_bytes());
    out.reserve(col.len() * T::SIZE);
    for x in col {
        x.put(out);
    }
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u64).to_le_bytes());
    out.extend_from_slice(b);
}

struct Cursor<'a> {
    b: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if n > self.b.len() {
            return Err(bad("unexpected end of data"));
        }
        let (head, tail) = self.b.split_at(n);
        self.b = tail;
        Ok(head)
    }

    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::get(self.take(8)?))
    }

    fn block(&mut self) -> Result<&'a [u8], Error> {
        let len = usize::try_from(self.u64()?).map_err(|_| bad("block too large"))?;
        self.take(len)
    }

    fn column<T: Le>(&mut self, rows: usize) -> Result<Vec<T>, Error> {
        let blk = self.block()?;
        if rows.checked_mul(T::SIZE) != Some(blk.len()) {
            return Err(bad("column length does not match row count"));
        }
        Ok(blk.chunks_exact(T::SIZE).map(T::get).collect())
    }
}

pub fn encode_index(header: &IndexHeader, index: &mut DeviceIndex) -> Result<Vec<u8>, Error> {
    index.compact();
    let mut hdr = header.clone();
    hdr.format_version = INDEX_FORMAT_VERSION;
    hdr.rows = index.len() as u64;
    let hdr_json = serde_json::to_vec(&hdr)?;

    let mut body =
        Vec::with_capacity(index.len() * 48 + index.names.len() + index.lower.len() + 64);
    put_col(&mut body, &index.ids);
    put_col(&mut body, &index.parents);
    put_col(&mut body, &index.name_off);
    put_col(&mut body, &index.name_len);
    put_col(&mut body, &index.lower_off);
    put_col(&mut body, &index.lower_len);
    put_col(&mut body, &index.sizes);
    put_col(&mut body, &index.modified);
    put_col(&mut body, &index.created);
    put_col(&mut body, &index.attrs);
    put_bytes(&mut body, index.names.as_bytes());
    put_bytes(&mut body, index.lower.as_bytes());
    body.extend_from_slice(&index.root_id.to_le_bytes());
    let packed = zstd::encode_all(&body[..], ZSTD_LEVEL)?;

    let mut out = Vec::with_capacity(16 + hdr_json.len() + packed.len());
    out.extend_from_slice(INDEX_MAGIC);
    out.extend_from_slice(&INDEX_FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&(hdr_json.len() as u32).to_le_bytes());
    out.extend_from_slice(&hdr_json);
    out.extend_from_slice(&packed);
    Ok(out)
}

fn check_arena_refs(offs: &[u32], lens: &[u16], arena: &str, what: &str) -> Result<(), Error> {
    for (o, l) in offs.iter().zip(lens) {
        let (o, e) = (*o as usize, *o as usize + *l as usize);
        if e > arena.len() || !arena.is_char_boundary(o) || !arena.is_char_boundary(e) {
            return Err(bad(format!("{what} offset out of range")));
        }
    }
    Ok(())
}

pub fn decode_index(bytes: &[u8]) -> Result<(IndexHeader, DeviceIndex), Error> {
    let mut cur = Cursor { b: bytes };
    if cur.take(8)? != INDEX_MAGIC {
        return Err(bad("bad magic"));
    }
    let version = u32::get(cur.take(4)?);
    if version != INDEX_FORMAT_VERSION {
        return Err(bad(format!("unsupported version {version}")));
    }
    let hdr_len = u32::get(cur.take(4)?) as usize;
    let header: IndexHeader =
        serde_json::from_slice(cur.take(hdr_len)?).map_err(|e| bad(format!("bad header: {e}")))?;
    let body = zstd::decode_all(cur.b).map_err(|e| bad(format!("bad body: {e}")))?;

    let rows = usize::try_from(header.rows).map_err(|_| bad("row count too large"))?;
    let mut c = Cursor { b: &body };
    let ids = c.column::<u64>(rows)?;
    let parents = c.column::<u32>(rows)?;
    let name_off = c.column::<u32>(rows)?;
    let name_len = c.column::<u16>(rows)?;
    let lower_off = c.column::<u32>(rows)?;
    let lower_len = c.column::<u16>(rows)?;
    let sizes = c.column::<u64>(rows)?;
    let modified = c.column::<i64>(rows)?;
    let created = c.column::<i64>(rows)?;
    let attrs = c.column::<u32>(rows)?;
    let names =
        String::from_utf8(c.block()?.to_vec()).map_err(|_| bad("names arena is not utf-8"))?;
    let lower =
        String::from_utf8(c.block()?.to_vec()).map_err(|_| bad("lower arena is not utf-8"))?;
    let root_id = c.u64()?;
    if !c.b.is_empty() {
        return Err(bad("trailing data"));
    }
    check_arena_refs(&name_off, &name_len, &names, "name")?;
    check_arena_refs(&lower_off, &lower_len, &lower, "lower")?;
    if parents
        .iter()
        .any(|p| *p != crate::index::NO_PARENT && *p as usize >= rows)
    {
        return Err(bad("parent row out of range"));
    }
    let mut index = DeviceIndex::from_columns(
        root_id, ids, parents, name_off, name_len, lower_off, lower_len, sizes, modified, created,
        attrs, names, lower,
    )
    .ok_or_else(|| bad("duplicate entry ids"))?;
    index.checkpoint = header.checkpoint;
    Ok((header, index))
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Write to `<path>.tmp`, then rename over `path`.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let tmp = tmp_path(path);
    let res = std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(res?)
}

pub fn save_index(path: &Path, header: &IndexHeader, index: &mut DeviceIndex) -> Result<(), Error> {
    let bytes = encode_index(header, index)?;
    write_atomic(path, &bytes)
}

pub fn load_index(path: &Path) -> Result<(IndexHeader, DeviceIndex), Error> {
    decode_index(&std::fs::read(path)?)
}
