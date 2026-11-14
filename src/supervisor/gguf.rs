use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

const MAX_STRING_LEN: u64 = 1024 * 1024; // 1 MiB
const MAX_ARRAY_LEN: u64 = 10_000_000;
const MAX_KV_COUNT: u64 = 100_000;
const MAX_TENSOR_COUNT: u64 = 1_000_000;
const MAX_READ_BYTES: u64 = 16 * 1024 * 1024; // 16 MiB
const MAX_ARRAY_DEPTH: usize = 8;

/// Metadata and parameter count extracted from a GGUF v2/v3 file header.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GgufInfo {
    pub architecture: Option<String>,
    pub name: Option<String>,
    pub size_label: Option<String>,
    pub general_type: Option<String>,
    pub file_type: Option<u32>,
    pub parameter_count: u64,
    pub has_pooling: bool,
}

/// Maps a GGUF `general.file_type` integer to a short quantization label.
pub fn file_type_label(ft: u32) -> Option<&'static str> {
    match ft {
        0 => Some("F32"),
        1 => Some("F16"),
        2 => Some("Q4_0"),
        3 => Some("Q4_1"),
        7 => Some("Q8_0"),
        8 => Some("Q5_0"),
        9 => Some("Q5_1"),
        10 => Some("Q2_K"),
        11 => Some("Q3_K_S"),
        12 => Some("Q3_K_M"),
        13 => Some("Q3_K_L"),
        14 => Some("Q4_K_S"),
        15 => Some("Q4_K_M"),
        16 => Some("Q5_K_S"),
        17 => Some("Q5_K_M"),
        18 => Some("Q6_K"),
        32 => Some("BF16"),
        _ => None,
    }
}

struct LimitedReader<R> {
    inner: R,
    read: u64,
    limit: u64,
}

impl<R: Read> LimitedReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            read: 0,
            limit,
        }
    }

    fn read_exact_bytes(&mut self, buf: &mut [u8]) -> Result<()> {
        let len = buf.len() as u64;
        if self.read.saturating_add(len) > self.limit {
            bail!(
                "GGUF header exceeds {} MiB limit",
                self.limit / (1024 * 1024)
            );
        }
        self.inner.read_exact(buf)?;
        self.read += len;
        Ok(())
    }

    fn skip_exact(&mut self, mut len: u64) -> Result<()> {
        if self.read.saturating_add(len) > self.limit {
            bail!(
                "GGUF header exceeds {} MiB limit",
                self.limit / (1024 * 1024)
            );
        }
        let mut scratch = [0u8; 4096];
        while len > 0 {
            let chunk = (len as usize).min(scratch.len());
            self.inner.read_exact(&mut scratch[..chunk])?;
            self.read += chunk as u64;
            len -= chunk as u64;
        }
        Ok(())
    }

    fn read_u8(&mut self) -> Result<u8> {
        let mut b = [0u8; 1];
        self.read_exact_bytes(&mut b)?;
        Ok(b[0])
    }

    fn read_u32(&mut self) -> Result<u32> {
        let mut b = [0u8; 4];
        self.read_exact_bytes(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn read_i32(&mut self) -> Result<i32> {
        let mut b = [0u8; 4];
        self.read_exact_bytes(&mut b)?;
        Ok(i32::from_le_bytes(b))
    }

    fn read_u64(&mut self) -> Result<u64> {
        let mut b = [0u8; 8];
        self.read_exact_bytes(&mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    fn read_i64(&mut self) -> Result<i64> {
        let mut b = [0u8; 8];
        self.read_exact_bytes(&mut b)?;
        Ok(i64::from_le_bytes(b))
    }

    fn read_u16(&mut self) -> Result<u16> {
        let mut b = [0u8; 2];
        self.read_exact_bytes(&mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    fn read_i16(&mut self) -> Result<i16> {
        let mut b = [0u8; 2];
        self.read_exact_bytes(&mut b)?;
        Ok(i16::from_le_bytes(b))
    }

    fn read_string(&mut self) -> Result<String> {
        let len = self.read_u64()?;
        if len > MAX_STRING_LEN {
            bail!("GGUF string length {len} exceeds 1 MiB limit");
        }
        let mut buf = vec![0u8; len as usize];
        self.read_exact_bytes(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    fn skip_string(&mut self) -> Result<()> {
        let len = self.read_u64()?;
        if len > MAX_STRING_LEN {
            bail!("GGUF string length {len} exceeds 1 MiB limit");
        }
        self.skip_exact(len)
    }

    fn skip_value(&mut self, vtype: u32, depth: usize) -> Result<()> {
        match vtype {
            0 | 1 | 7 => self.skip_exact(1),
            2 | 3 => self.skip_exact(2),
            4..=6 => self.skip_exact(4),
            10..=12 => self.skip_exact(8),
            8 => self.skip_string(),
            9 => {
                if depth >= MAX_ARRAY_DEPTH {
                    bail!("GGUF nested array depth exceeds limit");
                }
                let elem_type = self.read_u32()?;
                let count = self.read_u64()?;
                if count > MAX_ARRAY_LEN {
                    bail!("GGUF array length {count} exceeds 10M limit");
                }
                if let Some(elem_size) = scalar_size(elem_type) {
                    let total = count
                        .checked_mul(elem_size)
                        .context("GGUF array byte length overflow")?;
                    self.skip_exact(total)
                } else {
                    for _ in 0..count {
                        self.skip_value(elem_type, depth + 1)?;
                    }
                    Ok(())
                }
            }
            other => bail!("Unknown GGUF value type: {other}"),
        }
    }

    fn read_int_as_i64(&mut self, vtype: u32) -> Result<Option<i64>> {
        match vtype {
            0 => Ok(Some(self.read_u8()? as i64)),
            1 => Ok(Some(self.read_u8()? as i8 as i64)),
            2 => Ok(Some(self.read_u16()? as i64)),
            3 => Ok(Some(self.read_i16()? as i64)),
            4 => Ok(Some(self.read_u32()? as i64)),
            5 => Ok(Some(self.read_i32()? as i64)),
            10 => Ok(Some(self.read_u64()? as i64)),
            11 => Ok(Some(self.read_i64()?)),
            _ => {
                self.skip_value(vtype, 0)?;
                Ok(None)
            }
        }
    }
}

fn scalar_size(vtype: u32) -> Option<u64> {
    match vtype {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        _ => None,
    }
}

/// Reads GGUF v2/v3 metadata and tensor parameter count from `r`.
pub fn read_gguf_info<R: Read>(r: R) -> Result<GgufInfo> {
    let mut reader = LimitedReader::new(r, MAX_READ_BYTES);
    let mut magic = [0u8; 4];
    reader.read_exact_bytes(&mut magic)?;
    if &magic != b"GGUF" {
        bail!("Invalid GGUF magic");
    }

    let version = reader.read_u32()?;
    if version != 2 && version != 3 {
        bail!("Unsupported GGUF version: {version}");
    }

    let tensor_count = reader.read_u64()?;
    if tensor_count > MAX_TENSOR_COUNT {
        bail!("GGUF tensor count {tensor_count} exceeds 1M limit");
    }

    let kv_count = reader.read_u64()?;
    if kv_count > MAX_KV_COUNT {
        bail!("GGUF KV count {kv_count} exceeds 100k limit");
    }

    let mut info = GgufInfo::default();

    for _ in 0..kv_count {
        let key = reader.read_string()?;
        let vtype = reader.read_u32()?;
        match key.as_str() {
            "general.architecture" if vtype == 8 => {
                info.architecture = Some(reader.read_string()?);
            }
            "general.name" if vtype == 8 => {
                info.name = Some(reader.read_string()?);
            }
            "general.size_label" if vtype == 8 => {
                info.size_label = Some(reader.read_string()?);
            }
            "general.type" if vtype == 8 => {
                info.general_type = Some(reader.read_string()?);
            }
            "general.file_type" => {
                if let Some(v) = reader.read_int_as_i64(vtype)? {
                    if let Ok(u) = u32::try_from(v) {
                        info.file_type = Some(u);
                    }
                }
            }
            k if k.ends_with(".pooling_type") => {
                if let Some(v) = reader.read_int_as_i64(vtype)? {
                    if v != 0 {
                        info.has_pooling = true;
                    }
                }
            }
            _ => {
                reader.skip_value(vtype, 0)?;
            }
        }
    }

    let mut total_params: u64 = 0;
    for _ in 0..tensor_count {
        reader.skip_string()?;
        let n_dims = reader.read_u32()?;
        if n_dims > 64 {
            bail!("GGUF tensor n_dims {n_dims} exceeds sanity limit");
        }
        let mut prod: u64 = if n_dims == 0 { 0 } else { 1 };
        for _ in 0..n_dims {
            let dim = reader.read_u64()?;
            prod = prod.saturating_mul(dim);
        }
        let _tensor_type = reader.read_u32()?;
        let _offset = reader.read_u64()?;
        total_params = total_params.saturating_add(prod);
    }
    info.parameter_count = total_params;

    Ok(info)
}

/// Reads GGUF v2/v3 metadata from a file on disk, capped at 16 MiB.
pub fn read_gguf_info_path(path: &Path) -> Result<GgufInfo> {
    let file = File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let limited = BufReader::new(file).take(MAX_READ_BYTES);
    read_gguf_info(limited)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) struct GgufBuilder {
        kvs: Vec<(String, u32, Vec<u8>)>,
        tensors: Vec<(String, Vec<u64>)>,
        version: u32,
        magic: [u8; 4],
        override_kv_count: Option<u64>,
    }

    impl GgufBuilder {
        pub(crate) fn new() -> Self {
            Self {
                kvs: Vec::new(),
                tensors: Vec::new(),
                version: 3,
                magic: *b"GGUF",
                override_kv_count: None,
            }
        }

        pub(crate) fn magic(mut self, magic: [u8; 4]) -> Self {
            self.magic = magic;
            self
        }

        pub(crate) fn version(mut self, version: u32) -> Self {
            self.version = version;
            self
        }

        pub(crate) fn kv_count_override(mut self, count: u64) -> Self {
            self.override_kv_count = Some(count);
            self
        }

        fn push_str(buf: &mut Vec<u8>, s: &str) {
            buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }

        pub(crate) fn kv_str(mut self, k: &str, v: &str) -> Self {
            let mut val = Vec::new();
            Self::push_str(&mut val, v);
            self.kvs.push((k.to_string(), 8, val));
            self
        }

        pub(crate) fn kv_u32(mut self, k: &str, v: u32) -> Self {
            self.kvs.push((k.to_string(), 4, v.to_le_bytes().to_vec()));
            self
        }

        pub(crate) fn kv_array_str(mut self, k: &str, vals: &[&str]) -> Self {
            let mut val = Vec::new();
            val.extend_from_slice(&8u32.to_le_bytes());
            val.extend_from_slice(&(vals.len() as u64).to_le_bytes());
            for s in vals {
                Self::push_str(&mut val, s);
            }
            self.kvs.push((k.to_string(), 9, val));
            self
        }

        pub(crate) fn tensor(mut self, name: &str, dims: &[u64]) -> Self {
            self.tensors.push((name.to_string(), dims.to_vec()));
            self
        }

        pub(crate) fn bytes(&self) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&self.magic);
            out.extend_from_slice(&self.version.to_le_bytes());
            out.extend_from_slice(&(self.tensors.len() as u64).to_le_bytes());
            let kv_count = self.override_kv_count.unwrap_or(self.kvs.len() as u64);
            out.extend_from_slice(&kv_count.to_le_bytes());
            for (k, vtype, vbytes) in &self.kvs {
                Self::push_str(&mut out, k);
                out.extend_from_slice(&vtype.to_le_bytes());
                out.extend_from_slice(vbytes);
            }
            for (name, dims) in &self.tensors {
                Self::push_str(&mut out, name);
                out.extend_from_slice(&(dims.len() as u32).to_le_bytes());
                for d in dims {
                    out.extend_from_slice(&d.to_le_bytes());
                }
                out.extend_from_slice(&0u32.to_le_bytes()); // tensor type
                out.extend_from_slice(&0u64.to_le_bytes()); // offset
            }
            out
        }
    }

    #[test]
    fn reads_metadata_and_counts_params() {
        let bytes = GgufBuilder::new()
            .kv_str("general.architecture", "llama")
            .kv_str("general.name", "Tiny")
            .kv_str("general.size_label", "1.7B")
            .kv_u32("general.file_type", 15)
            .kv_array_str("tokenizer.ggml.tokens", &["a", "b", "c"])
            .tensor("tok_embd.weight", &[4096, 32000])
            .tensor("output_norm.weight", &[4096])
            .bytes();

        let info = read_gguf_info(bytes.as_slice()).unwrap();
        assert_eq!(info.architecture.as_deref(), Some("llama"));
        assert_eq!(info.name.as_deref(), Some("Tiny"));
        assert_eq!(info.size_label.as_deref(), Some("1.7B"));
        assert_eq!(info.file_type, Some(15));
        assert_eq!(info.parameter_count, 131_076_096);
        assert_eq!(file_type_label(15), Some("Q4_K_M"));
    }

    #[test]
    fn detects_pooling() {
        let with_pooling = GgufBuilder::new()
            .kv_str("general.architecture", "bert")
            .kv_u32("bert.pooling_type", 1)
            .bytes();
        assert!(read_gguf_info(with_pooling.as_slice()).unwrap().has_pooling);

        let zero_pooling = GgufBuilder::new()
            .kv_str("general.architecture", "bert")
            .kv_u32("bert.pooling_type", 0)
            .bytes();
        assert!(!read_gguf_info(zero_pooling.as_slice()).unwrap().has_pooling);
    }

    #[test]
    fn rejects_bad_magic_and_v1() {
        let bad_magic = GgufBuilder::new().magic(*b"GGML").bytes();
        assert!(read_gguf_info(bad_magic.as_slice()).is_err());

        let v1 = GgufBuilder::new().version(1).bytes();
        assert!(read_gguf_info(v1.as_slice()).is_err());
    }

    #[test]
    fn truncated_input_errors_without_panic() {
        let bytes = GgufBuilder::new()
            .kv_str("general.architecture", "llama")
            .kv_u32("general.file_type", 15)
            .kv_array_str("tokenizer.ggml.tokens", &["a", "b", "c"])
            .tensor("tok_embd.weight", &[4096, 32000])
            .bytes();

        for len in 0..bytes.len() {
            let res = read_gguf_info(&bytes[..len]);
            assert!(res.is_err(), "expected error at truncated len {len}");
        }
    }

    #[test]
    fn limits_enforced() {
        // String len of 2 MiB -> Err
        let mut big_str = Vec::new();
        big_str.extend_from_slice(b"GGUF");
        big_str.extend_from_slice(&3u32.to_le_bytes());
        big_str.extend_from_slice(&0u64.to_le_bytes());
        big_str.extend_from_slice(&1u64.to_le_bytes());
        big_str.extend_from_slice(&((2 * 1024 * 1024) as u64).to_le_bytes());
        assert!(read_gguf_info(big_str.as_slice()).is_err());

        // kv_count 200_000 -> Err
        let many_kvs = GgufBuilder::new().kv_count_override(200_000).bytes();
        assert!(read_gguf_info(many_kvs.as_slice()).is_err());
    }
}
