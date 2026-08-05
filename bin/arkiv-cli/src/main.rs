mod simulate;

use alloy_network::EthereumWallet;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types::eth::Log as RpcLog;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolEvent;
use arkiv_bindings::*;
use arkiv_constants::{ADDRESS_LEN, WORD_LEN};
use clap::{Parser, Subcommand};
use eyre::{Result, bail};
use rand::Rng;
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// CLI for submitting EntityRegistry operations.
#[derive(Parser)]
#[command(name = "arkiv-cli", version)]
struct Cli {
    /// RPC endpoint URL.
    #[arg(long, default_value = "http://localhost:8545")]
    rpc_url: String,

    /// Private key for signing transactions (hex, with or without 0x prefix).
    /// Defaults to the first test mnemonic account.
    #[arg(
        long,
        default_value = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
    )]
    private_key: String,

    /// Arkiv precompile address.
    #[arg(long, default_value = "0x4400000000000000000000000000000000000044")]
    registry: Address,

    /// Assumed block time for duration-to-block conversion (e.g. "2s").
    #[arg(long, default_value = "2s", value_parser = humantime::parse_duration)]
    block_time: Duration,

    /// Gas price in wei.
    #[arg(long, default_value = "1000000000")]
    gas_price: u128,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create an entity. Either `--payload` or `--random-payload` must be set.
    Create {
        /// Content type MIME string.
        #[arg(long, default_value = "application/octet-stream")]
        content_type: String,

        /// Absolute expiry block. 0 means "purely relative" — use --min-lifetime.
        #[arg(long, default_value_t = 0)]
        expires_at: u64,

        /// Minimum lifetime in blocks from now. Resolves as
        /// max(--expires-at, current + --min-lifetime).
        #[arg(long, default_value_t = 0)]
        min_lifetime: u64,

        /// Make the entity read-only: it can never be patched.
        #[arg(long, default_value_t = false)]
        readonly: bool,

        /// Let anyone (not just the owner) extend the entity's expiry.
        #[arg(long, default_value_t = false)]
        permissionless_extension: bool,

        /// Key salt. Only affects predictability, never uniqueness.
        #[arg(long, default_value_t = 0)]
        salt: u128,

        /// Payload bytes. Raw string by default; 0x-prefixed values are decoded as hex bytes.
        /// Mutually exclusive with `--random-payload`.
        #[arg(long)]
        payload: Option<String>,

        /// Generate a random payload of `--size` bytes instead of using `--payload`.
        #[arg(long, default_value_t = false)]
        random_payload: bool,

        /// Random payload size in bytes (only used with `--random-payload`).
        #[arg(long, default_value = "256")]
        size: usize,

        /// Comma-separated attributes: name=value, name:string=value, name:uint=value, name:entityKey=0x...
        #[arg(long, default_value = "")]
        attributes: String,
    },

    /// Patch an entity: set some attributes, unset others, leave the rest alone.
    ///
    /// Unlike the old whole-entity update, anything you do not mention keeps
    /// its current value.
    Patch {
        /// Entity key to patch.
        #[arg(long)]
        key: B256,

        /// Content type MIME string. Omit to leave it unchanged.
        #[arg(long)]
        content_type: Option<String>,

        /// Payload bytes. Raw string by default; 0x-prefixed values are decoded as hex bytes.
        /// Omit to leave the payload unchanged.
        #[arg(long)]
        payload: Option<String>,

        /// Generate a random payload of `--size` bytes instead of using `--payload`.
        #[arg(long, default_value_t = false)]
        random_payload: bool,

        /// Random payload size in bytes (only used with `--random-payload`).
        #[arg(long, default_value = "256")]
        size: usize,

        /// Comma-separated attributes to set: name=value, name:string=value, ...
        #[arg(long, default_value = "")]
        attributes: String,

        /// Comma-separated *attribute* names to remove from the entity, e.g.
        /// `--unset color,size`. This deletes those attributes; it does not
        /// touch the entity's lifetime — use `delete` to remove the entity, or
        /// `extend-expiry` to change when it expires.
        ///
        /// On the wire each name becomes a tombstone: a `(name, typeId 0, "")`
        /// triple, the encoding for "unset this".
        #[arg(long, default_value = "")]
        unset: String,
    },

    /// Extend an entity's expiry. Never shortens: equal is a no-op, earlier reverts.
    ExtendExpiry {
        /// Entity key to extend.
        #[arg(long)]
        key: B256,

        /// Absolute expiry block. 0 means "purely relative" — use --min-lifetime.
        #[arg(long, default_value_t = 0)]
        expires_at: u64,

        /// Minimum lifetime in blocks from now.
        #[arg(long, default_value_t = 0)]
        min_lifetime: u64,
    },

    /// Transfer entity ownership.
    Transfer {
        /// Entity key to transfer.
        #[arg(long)]
        key: B256,

        /// New owner address.
        #[arg(long)]
        new_owner: Address,
    },

    /// Delete an entity.
    Delete {
        /// Entity key to delete.
        #[arg(long)]
        key: B256,
    },

    /// Print the current block number, its UNIX timestamp, and seconds since
    /// the previous block. Calls `arkiv_getBlockTiming` on the node.
    BlockTiming,

    /// Check an account's ETH balance.
    Balance {
        /// Address to check. Defaults to the signer's address.
        #[arg(long)]
        address: Option<Address>,
    },

    /// Submit a batch of operations from a JSON file in a single tx.
    /// See `scripts/fixtures/` for examples.
    Batch {
        /// Path to a JSON file containing an array of operations.
        file: PathBuf,
    },

    /// Splice Arkiv dev-funded accounts into a geth-format genesis JSON.
    ///
    /// Legacy command name; no bytecode is deployed at `ARKIV_ADDRESS`.
    InjectPredeploy {
        /// Input genesis JSON (geth format).
        file: PathBuf,

        /// Output path. Defaults to overwriting the input.
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Fire off multiple entity creates.
    /// Load-generate: fire `--count` single-create transactions back to back,
    /// one per tx rather than one batch.
    ///
    /// A throughput/backpressure probe, not a correctness tool — it is how the
    /// pool's behaviour under a burst gets exercised (nonce sequencing, the
    /// pool-full retry path). For a realistic mixed workload use `simulate`.
    Spam {
        /// Number of entities to create.
        #[arg(long, default_value = "10")]
        count: u32,

        /// Payload size in bytes per entity.
        #[arg(long, default_value = "256")]
        size: usize,

        /// Minimum lifetime in blocks for each created entity.
        #[arg(long)]
        min_lifetime: u64,
    },

    /// Continuously generate a weighted mix of entity operations against
    /// a running node, simulating live system traffic.
    Simulate(simulate::SimulateArgs),
}

/// The `$contentType` triple — a `str` attribute, since content type is a
/// user-managed system attribute rather than its own operation field now.
fn content_type_attr(s: &str) -> Result<Attribute> {
    Attribute::from_value(
        Ident32::system("$contentType")?,
        &arkiv_interfaces::entity::AttributeValue::Str(s.to_string()),
    )
    .map_err(|e| eyre::eyre!("content type: {e}"))
}

/// The `$payload` triple.
fn payload_attr(b: &Bytes) -> Result<Attribute> {
    Attribute::from_value(
        Ident32::system("$payload")?,
        &arkiv_interfaces::entity::AttributeValue::Bytes(b.to_vec()),
    )
    .map_err(|e| eyre::eyre!("payload: {e}"))
}

/// Pack the `--readonly` / `--permissionless-extension` switches into the
/// creation-flags byte.
fn creation_flags(readonly: bool, permissionless_extension: bool) -> u8 {
    let mut flags = CreationFlags::NONE;
    if readonly {
        flags = flags | CreationFlags::READONLY;
    }
    if permissionless_extension {
        flags = flags | CreationFlags::PERMISSIONLESS_EXTENSION;
    }
    flags.bits()
}

/// Parse `--unset a,b,c` into tombstone triples — one per attribute to remove.
fn parse_unset(input: &str) -> Result<Vec<Attribute>> {
    input
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|name| Ok(Attribute::tombstone(Ident32::encode(name)?)))
        .collect()
}

fn random_payload(size: usize) -> Bytes {
    let mut rng = rand::rng();
    let mut buf = vec![0u8; size];
    rng.fill(&mut buf[..]);
    Bytes::from(buf)
}

/// Predict the entity key the `n`-th CREATE will mint, mirroring the node's
/// `derive_entity_key`: keccak over (chain id, registry address, owner, nonce).
fn predict_entity_key(
    chain_id: u64,
    registry: Address,
    owner: Address,
    nonce: u64,
    salt: u128,
) -> B256 {
    // Must match `arkiv_reth_executor::decode::derive_entity_key` byte for byte:
    // chain_id ‖ registry ‖ owner ‖ nonce ‖ salt.
    let mut buf = Vec::with_capacity(
        WORD_LEN + ADDRESS_LEN + ADDRESS_LEN + size_of::<u64>() + size_of::<u128>(),
    );
    buf.extend_from_slice(&U256::from(chain_id).to_be_bytes::<32>());
    buf.extend_from_slice(registry.as_slice());
    buf.extend_from_slice(owner.as_slice());
    buf.extend_from_slice(&nonce.to_be_bytes());
    buf.extend_from_slice(&salt.to_be_bytes());
    keccak256(&buf)
}

/// Print the per-op events a receipt carries. One event type per op kind now,
/// so this tries each in turn rather than switching on a discriminator.
fn print_events(logs: &[RpcLog]) {
    use IEntityRegistry as E;
    for log in logs {
        println!("---");
        if let Ok(ev) = E::EntityCreated::decode_log(&log.inner) {
            println!("  op:          CREATE");
            println!("  entity_key:  {}", ev.data.entityKey);
            println!("  owner:       {}", ev.data.owner);
            println!("  expires_at:  {}", ev.data.expiresAt);
            println!("  flags:       0b{:08b}", ev.data.creationFlags);
        } else if let Ok(ev) = E::EntityPatched::decode_log(&log.inner) {
            println!("  op:          PATCH");
            println!("  entity_key:  {}", ev.data.entityKey);
            println!("  owner:       {}", ev.data.owner);
        } else if let Ok(ev) = E::ExpiryExtended::decode_log(&log.inner) {
            println!("  op:          EXTEND_EXPIRY");
            println!("  entity_key:  {}", ev.data.entityKey);
            println!("  expires_at:  {}", ev.data.expiresAt);
        } else if let Ok(ev) = E::OwnershipTransferred::decode_log(&log.inner) {
            println!("  op:          TRANSFER_OWNERSHIP");
            println!("  entity_key:  {}", ev.data.entityKey);
            println!("  from:        {}", ev.data.previousOwner);
            println!("  to:          {}", ev.data.newOwner);
        } else if let Ok(ev) = E::EntityDeleted::decode_log(&log.inner) {
            println!("  op:          DELETE");
            println!("  entity_key:  {}", ev.data.entityKey);
            println!("  owner:       {}", ev.data.owner);
        } else {
            println!("  (unrecognised event)");
        }
    }
}

// ---------------------------------------------------------------------------
// Batch JSON schema
// ---------------------------------------------------------------------------

/// An entity-key field in a batch op. Either a hex literal (`"0x..."`) or a
/// reference (`"$N"`) to the Nth op in the batch (which must be a CREATE).
#[derive(Debug, Clone)]
enum EntityKeyRef {
    Literal(B256),
    Ref(usize),
}

impl<'de> Deserialize<'de> for EntityKeyRef {
    fn deserialize<D: Deserializer<'de>>(de: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        if let Some(rest) = s.strip_prefix('$') {
            let idx: usize = rest.parse().map_err(serde::de::Error::custom)?;
            Ok(EntityKeyRef::Ref(idx))
        } else {
            let key = s.parse::<B256>().map_err(serde::de::Error::custom)?;
            Ok(EntityKeyRef::Literal(key))
        }
    }
}

fn default_content_type() -> String {
    "application/octet-stream".to_string()
}

/// One attribute in a batch JSON op. The value type is discriminated by
/// which of `string` / `uint` / `entityKey` is present (untagged enum).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BatchAttribute {
    /// `Ident32` name (lowercase ASCII, validated client-side).
    name: String,
    #[serde(flatten)]
    value: BatchAttributeValue,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BatchAttributeValue {
    String {
        string: String,
    },
    Bool {
        bool: bool,
    },
    Int {
        int: i32,
    },
    Uint {
        uint: U256,
    },
    /// A fixed-18-decimal value, as a decimal string (`"1.5"`).
    Decimal {
        decimal: String,
    },
    Bytes32 {
        bytes32: B256,
    },
    Address {
        address: Address,
    },
    EntityKey {
        #[serde(rename = "entityKey")]
        entity_key: EntityKeyRef,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum BatchOp {
    Create {
        #[serde(default = "default_content_type", rename = "contentType")]
        content_type: String,
        /// Optional payload string. If prefixed with `0x` decoded as hex,
        /// otherwise treated as raw UTF-8 bytes. Mutually exclusive with `size`.
        payload: Option<String>,
        /// Random payload size in bytes. Mutually exclusive with `payload`.
        size: Option<usize>,
        #[serde(default, rename = "expiresAt")]
        expires_at: u64,
        #[serde(default, rename = "minLifetime")]
        min_lifetime: u64,
        #[serde(default)]
        salt: u128,
        #[serde(default, rename = "creationFlags")]
        creation_flags: u8,
        #[serde(default)]
        attributes: Vec<BatchAttribute>,
    },
    Patch {
        #[serde(rename = "entityKey")]
        entity_key: EntityKeyRef,
        #[serde(default, rename = "contentType")]
        content_type: Option<String>,
        payload: Option<String>,
        size: Option<usize>,
        #[serde(default)]
        attributes: Vec<BatchAttribute>,
        /// Attribute names to unset.
        #[serde(default)]
        unset: Vec<String>,
    },
    ExtendExpiry {
        #[serde(rename = "entityKey")]
        entity_key: EntityKeyRef,
        #[serde(default, rename = "expiresAt")]
        expires_at: u64,
        #[serde(default, rename = "minLifetime")]
        min_lifetime: u64,
    },
    Transfer {
        #[serde(rename = "entityKey")]
        entity_key: EntityKeyRef,
        #[serde(rename = "newOwner")]
        new_owner: Address,
    },
    Delete {
        #[serde(rename = "entityKey")]
        entity_key: EntityKeyRef,
    },
}

/// Build a sol `Attribute` from a batch entry, validating the Ident32 name.
fn build_attribute(
    attr: &BatchAttribute,
    resolve: &impl Fn(&EntityKeyRef) -> Result<B256>,
) -> Result<Attribute> {
    let name = Ident32::encode(&attr.name)
        .map_err(|e| eyre::eyre!("invalid attribute name '{}': {}", attr.name, e))?;
    let value = match &attr.value {
        BatchAttributeValue::Bool { bool } => AttributeValue::Bool(*bool),
        BatchAttributeValue::Int { int } => AttributeValue::Int(*int),
        BatchAttributeValue::Uint { uint } => AttributeValue::U256(uint.to_be_bytes()),
        BatchAttributeValue::Decimal { decimal } => parse_decimal_value(decimal)?,
        BatchAttributeValue::Bytes32 { bytes32 } => AttributeValue::Bytes32(bytes32.0),
        BatchAttributeValue::String { string } => AttributeValue::Str(string.clone()),
        BatchAttributeValue::Address { address } => {
            AttributeValue::EthereumAddress(address.into_array())
        }
        BatchAttributeValue::EntityKey { entity_key } => {
            AttributeValue::EntityKey(resolve(entity_key)?.0)
        }
    };
    Attribute::from_value(name, &value)
        .map_err(|e| eyre::eyre!("invalid value for attribute '{}': {}", attr.name, e))
}

/// Parse a decimal string (`"-1.5"`) into a fixed-scale `decimal` value: the
/// number multiplied by `10^DECIMAL_SCALE`, as a two's-complement `int256`.
fn parse_decimal_value(text: &str) -> Result<AttributeValue> {
    let (negative, digits) = split_sign(text);
    let magnitude = parse_scaled_magnitude(digits)
        .map_err(|e| eyre::eyre!("invalid decimal value '{}': {}", text, e))?;
    let signed = if negative {
        U256::ZERO.wrapping_sub(magnitude)
    } else {
        magnitude
    };
    Ok(AttributeValue::Decimal(signed.to_be_bytes()))
}

/// Split a leading sign off a decimal literal.
fn split_sign(text: &str) -> (bool, &str) {
    match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    }
}

/// `"1.5"` → `1_500_000_000_000_000_000`: the digits with the decimal point
/// removed, right-padded to exactly [`DECIMAL_SCALE`] fractional places.
fn parse_scaled_magnitude(digits: &str) -> Result<U256> {
    let scale = DECIMAL_SCALE as usize;
    let (whole, frac) = digits.split_once('.').unwrap_or((digits, ""));
    if frac.len() > scale {
        bail!("more than {scale} decimal places");
    }
    if whole.is_empty() && frac.is_empty() {
        bail!("no digits");
    }
    let mantissa = format!(
        "{}{frac}{}",
        if whole.is_empty() { "0" } else { whole },
        "0".repeat(scale - frac.len()),
    );
    mantissa.parse::<U256>().map_err(Into::into)
}

/// Build the contract's `Attribute[]` from batch entries, sorted by name
/// ascending as the contract requires for deterministic hashing.
fn build_attributes(
    attrs: &[BatchAttribute],
    resolve: &impl Fn(&EntityKeyRef) -> Result<B256>,
) -> Result<Vec<Attribute>> {
    let mut out: Vec<Attribute> = attrs
        .iter()
        .map(|a| build_attribute(a, resolve))
        .collect::<Result<_>>()?;
    Attribute::sort(&mut out);
    Ok(out)
}

fn build_cli_attributes(input: &str, command_name: &str) -> Result<Vec<Attribute>> {
    let attrs = parse_cli_attributes(input)?;
    let resolve = |r: &EntityKeyRef| -> Result<B256> {
        match r {
            EntityKeyRef::Literal(k) => Ok(*k),
            EntityKeyRef::Ref(i) => {
                bail!(
                    "${} references are only supported in batch JSON files, not {} --attributes",
                    i,
                    command_name
                )
            }
        }
    };
    build_attributes(&attrs, &resolve)
}

fn parse_cli_attributes(input: &str) -> Result<Vec<BatchAttribute>> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(Vec::new());
    }

    let mut attrs = Vec::new();
    for (idx, raw) in split_cli_attributes(input)?.into_iter().enumerate() {
        let attr = parse_cli_attribute(raw.trim()).map_err(|e| {
            eyre::eyre!(
                "invalid attribute #{} ('{}'): {}\nexpected one of: name=value, name:string=value, name:uint=value, name:entityKey=0x<64 hex chars>\nstrings may be quoted with single or double quotes; use backslash to escape quotes, commas, or backslashes inside quoted strings",
                idx + 1,
                raw.trim(),
                e
            )
        })?;
        attrs.push(attr);
    }
    Ok(attrs)
}

fn split_cli_attributes(input: &str) -> Result<Vec<&str>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;

    for (idx, ch) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote.is_some() && ch == '\\' {
            escaped = true;
            continue;
        }
        if Some(ch) == quote {
            quote = None;
            continue;
        }
        if quote.is_none() && (ch == '\'' || ch == '"') {
            quote = Some(ch);
            continue;
        }
        if quote.is_none() && ch == ',' {
            let part = input[start..idx].trim();
            if part.is_empty() {
                bail!("empty attribute before comma at byte {}", idx);
            }
            parts.push(part);
            start = idx + ch.len_utf8();
        }
    }

    if let Some(q) = quote {
        bail!("unterminated {}-quoted string in attributes", q);
    }

    let part = input[start..].trim();
    if part.is_empty() {
        bail!("empty attribute after final comma");
    }
    parts.push(part);
    Ok(parts)
}

fn parse_cli_attribute(raw: &str) -> Result<BatchAttribute> {
    let Some(eq) = raw.find('=') else {
        bail!("missing '=' separator");
    };

    let left = raw[..eq].trim();
    let value = raw[eq + 1..].trim();
    if left.is_empty() {
        bail!("missing attribute name before '='");
    }
    if value.is_empty() {
        bail!("missing value after '='");
    }

    let (name, ty) = match left.rsplit_once(':') {
        Some((name, ty)) => {
            let ty = ty.trim();
            if ty.is_empty() {
                bail!("missing type after ':'");
            }
            (name.trim(), Some(ty))
        }
        None => (left, None),
    };

    if name.is_empty() {
        bail!("missing attribute name before type");
    }

    let parsed_value = match ty {
        Some("string" | "str") => BatchAttributeValue::String {
            string: parse_cli_string_value(value)?,
        },
        Some("bool") => BatchAttributeValue::Bool {
            bool: value
                .parse()
                .map_err(|_| eyre::eyre!("invalid bool value '{}'; expected true or false", value))?,
        },
        Some("int") => BatchAttributeValue::Int {
            int: value
                .parse()
                .map_err(|e| eyre::eyre!("invalid int value '{}': {}", value, e))?,
        },
        Some("uint" | "u256") => BatchAttributeValue::Uint {
            uint: parse_cli_uint_value(value)?,
        },
        Some("decimal") => BatchAttributeValue::Decimal {
            decimal: value.to_string(),
        },
        Some("bytes32") => BatchAttributeValue::Bytes32 {
            bytes32: value
                .parse()
                .map_err(|e| eyre::eyre!("invalid bytes32 value '{}': {}", value, e))?,
        },
        Some("address") => BatchAttributeValue::Address {
            address: value
                .parse()
                .map_err(|e| eyre::eyre!("invalid address value '{}': {}", value, e))?,
        },
        Some("entityKey" | "entity-key" | "key") => BatchAttributeValue::EntityKey {
            entity_key: EntityKeyRef::Literal(parse_cli_entity_key_value(value)?),
        },
        Some(other) => bail!(
            "unknown attribute type '{}'; expected bool, int, uint, decimal, bytes32, string, address, or entityKey",
            other
        ),
        None if is_quoted(value) => BatchAttributeValue::String {
            string: parse_cli_string_value(value)?,
        },
        None if looks_like_entity_key(value) => BatchAttributeValue::EntityKey {
            entity_key: EntityKeyRef::Literal(parse_cli_entity_key_value(value)?),
        },
        None => BatchAttributeValue::Uint {
            uint: parse_cli_uint_value(value).map_err(|e| {
                eyre::eyre!(
                    "{}; unquoted shorthand values are parsed as uints. Quote strings, e.g. {}='{}', or use {}:string={}",
                    e,
                    name,
                    value,
                    name,
                    value
                )
            })?,
        },
    };

    Ok(BatchAttribute {
        name: name.to_string(),
        value: parsed_value,
    })
}

fn is_quoted(value: &str) -> bool {
    (value.starts_with('\'') && value.ends_with('\''))
        || (value.starts_with('"') && value.ends_with('"'))
}

fn looks_like_entity_key(value: &str) -> bool {
    value
        .strip_prefix("0x")
        .is_some_and(|hex| hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

fn parse_cli_string_value(value: &str) -> Result<String> {
    if !is_quoted(value) {
        return Ok(value.to_string());
    }

    let quote = value.as_bytes()[0] as char;
    let inner = &value[1..value.len() - 1];
    let mut out = String::new();
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            out.push(match ch {
                '\\' => '\\',
                '\'' => '\'',
                '"' => '"',
                ',' => ',',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => bail!("unsupported escape '\\{}' in string value", other),
            });
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    if escaped {
        bail!("string value ends with an unfinished escape");
    }
    if out.contains(quote) && !inner.contains('\\') {
        bail!("unescaped quote in string value");
    }
    Ok(out)
}

fn parse_cli_uint_value(value: &str) -> Result<U256> {
    value
        .parse::<U256>()
        .map_err(|e| eyre::eyre!("invalid uint value '{}': {}", value, e))
}

fn parse_cli_entity_key_value(value: &str) -> Result<B256> {
    value
        .parse::<B256>()
        .map_err(|e| eyre::eyre!("invalid entityKey '{}': {}", value, e))
}

/// Resolve `--payload` / `--random-payload` / `--size` flags into raw bytes.
/// Exactly one of `payload` or `random` must be set.
fn resolve_cli_payload(payload: Option<&str>, random: bool, size: usize) -> Result<Bytes> {
    match (payload, random) {
        (Some(_), true) => bail!("--payload and --random-payload are mutually exclusive"),
        (None, false) => bail!("either --payload or --random-payload must be provided"),
        (Some(s), false) => parse_cli_payload(s),
        (None, true) => Ok(random_payload(size)),
    }
}

fn parse_cli_payload(payload: &str) -> Result<Bytes> {
    if let Some(hex) = payload.strip_prefix("0x") {
        if hex.is_empty() {
            bail!(
                "invalid --payload '0x': hex payload is empty; pass a raw string for text payloads or 0x-prefixed even-length hex bytes"
            );
        }
        if hex.len() % 2 != 0 {
            bail!(
                "invalid --payload '{}': hex payload has {} digits, but byte hex must have an even number of digits",
                payload,
                hex.len()
            );
        }
        if let Some((idx, ch)) = hex.char_indices().find(|(_, ch)| !ch.is_ascii_hexdigit()) {
            bail!(
                "invalid --payload '{}': non-hex character '{}' at byte {} after the 0x prefix",
                payload,
                ch,
                idx
            );
        }
        return hex::decode(hex).map(Bytes::from).map_err(|e| {
            eyre::eyre!(
                "invalid --payload '{}': failed to decode hex: {}",
                payload,
                e
            )
        });
    }

    Ok(Bytes::from(payload.as_bytes().to_vec()))
}

/// Resolve `payload`/`size` fields into raw bytes.
fn resolve_payload(payload: Option<&str>, size: Option<usize>) -> Result<Bytes> {
    match (payload, size) {
        (Some(_), Some(_)) => bail!("payload and size are mutually exclusive"),
        (Some(s), None) => {
            if let Some(hex) = s.strip_prefix("0x") {
                Ok(Bytes::from(hex::decode(hex)?))
            } else {
                Ok(Bytes::from(s.as_bytes().to_vec()))
            }
        }
        (None, Some(n)) => Ok(random_payload(n)),
        (None, None) => Ok(Bytes::new()),
    }
}

/// Splice Arkiv's prefunded dev accounts into a geth-format genesis JSON.
///
/// Merges [`arkiv_genesis::genesis_alloc`] into the alloc: the
/// [`arkiv_genesis::ARKIV_DEV_ACCOUNT_COUNT`] mnemonic-derived dev
/// accounts, each prefunded with [`arkiv_genesis::arkiv_dev_balance_wei`].
/// The command name is legacy; no bytecode is deployed at
/// [`arkiv_genesis::ARKIV_ADDRESS`].
///
/// Output is pretty-printed back to disk (overwriting the input by
/// default, or to `out` if specified).
fn inject_predeploy(input: &std::path::Path, out: Option<&std::path::Path>) -> Result<()> {
    use arkiv_genesis::genesis_alloc;

    let raw = std::fs::read_to_string(input)
        .map_err(|e| eyre::eyre!("failed to read {}: {}", input.display(), e))?;
    let mut genesis: arkiv_genesis::Genesis = serde_json::from_str(&raw)
        .map_err(|e| eyre::eyre!("failed to parse {} as genesis JSON: {}", input.display(), e))?;

    let arkiv_alloc = genesis_alloc()?;
    let account_count = arkiv_alloc.len();
    for (addr, account) in arkiv_alloc {
        genesis.alloc.insert(addr, account);
    }

    let dest = out.unwrap_or(input);
    let serialized = serde_json::to_string_pretty(&genesis)?;
    std::fs::write(dest, serialized)
        .map_err(|e| eyre::eyre!("failed to write {}: {}", dest.display(), e))?;

    eprintln!(
        "injected {} Arkiv dev accounts into {}",
        account_count,
        dest.display(),
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // `inject-predeploy` is a pure JSON munger — no network, no signer.
    // Handle it before any of the provider setup below.
    if let Command::InjectPredeploy { file, out } = &cli.command {
        return inject_predeploy(file, out.as_deref());
    }

    // `simulate` builds its own multi-signer provider; bypass the
    // single-signer setup below.
    if let Command::Simulate(args) = cli.command {
        return simulate::run(
            args,
            &cli.rpc_url,
            cli.registry,
            cli.gas_price,
            cli.block_time,
        )
        .await;
    }

    let signer: PrivateKeySigner = cli.private_key.parse()?;
    let signer_address = signer.address();
    let wallet = EthereumWallet::from(signer);

    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(cli.rpc_url.parse()?);

    let registry = IEntityRegistry::new(cli.registry, &provider);

    match cli.command {
        Command::Create {
            content_type,
            expires_at,
            min_lifetime,
            readonly,
            permissionless_extension,
            salt,
            payload,
            random_payload,
            size,
            attributes,
        } => {
            let resolved_payload = resolve_cli_payload(payload.as_deref(), random_payload, size)?;
            let mut attributes = build_cli_attributes(&attributes, "create")?;
            attributes.push(content_type_attr(&content_type)?);
            attributes.push(payload_attr(&resolved_payload)?);
            let op = Operation::create(
                salt,
                expires_at,
                min_lifetime,
                creation_flags(readonly, permissionless_extension),
                attributes,
            );

            let receipt = registry
                .execute(vec![op])
                .gas_price(cli.gas_price)
                .send()
                .await?
                .get_receipt()
                .await?;
            println!("tx: {}", receipt.transaction_hash);
            print_events(receipt.inner.logs());
        }

        Command::Patch {
            key,
            content_type,
            payload,
            random_payload,
            size,
            attributes,
            unset,
        } => {
            let mut mutations = build_cli_attributes(&attributes, "patch")?;
            mutations.extend(parse_unset(&unset)?);
            if let Some(ct) = content_type.as_deref() {
                mutations.push(content_type_attr(ct)?);
            }
            // Only touch the payload if the caller asked to — omitting it must
            // leave the stored payload alone, not blank it.
            if payload.is_some() || random_payload {
                let resolved = resolve_cli_payload(payload.as_deref(), random_payload, size)?;
                mutations.push(payload_attr(&resolved)?);
            }
            if mutations.is_empty() {
                bail!(
                    "a patch needs at least one of --attributes, --unset, --content-type, --payload"
                );
            }
            let op = Operation::patch(key, mutations);

            let receipt = registry
                .execute(vec![op])
                .gas_price(cli.gas_price)
                .send()
                .await?
                .get_receipt()
                .await?;
            println!("tx: {}", receipt.transaction_hash);
            print_events(receipt.inner.logs());
        }

        Command::ExtendExpiry {
            key,
            expires_at,
            min_lifetime,
        } => {
            let op = Operation::extend_expiry(key, expires_at, min_lifetime);

            let receipt = registry
                .execute(vec![op])
                .gas_price(cli.gas_price)
                .send()
                .await?
                .get_receipt()
                .await?;
            println!("tx: {}", receipt.transaction_hash);
            print_events(receipt.inner.logs());
        }

        Command::Transfer { key, new_owner } => {
            let op = Operation::transfer_ownership(key, new_owner);

            let receipt = registry
                .execute(vec![op])
                .gas_price(cli.gas_price)
                .send()
                .await?
                .get_receipt()
                .await?;
            println!("tx: {}", receipt.transaction_hash);
            print_events(receipt.inner.logs());
        }

        Command::Delete { key } => {
            let op = Operation::delete(key);

            let receipt = registry
                .execute(vec![op])
                .gas_price(cli.gas_price)
                .send()
                .await?
                .get_receipt()
                .await?;
            println!("tx: {}", receipt.transaction_hash);
            print_events(receipt.inner.logs());
        }

        Command::BlockTiming => {
            #[derive(Debug, Deserialize)]
            struct BlockTiming {
                current_block: u64,
                current_block_time: u64,
                duration: u64,
            }
            let t: BlockTiming = provider
                .raw_request("arkiv_getBlockTiming".into(), ())
                .await?;
            println!("block:     {}", t.current_block);
            println!("timestamp: {}", t.current_block_time);
            println!("duration:  {}s", t.duration);
        }

        Command::InjectPredeploy { .. } => unreachable!("handled at top of main"),
        Command::Simulate(_) => unreachable!("handled at top of main"),

        Command::Batch { file } => {
            let json = std::fs::read_to_string(&file)
                .map_err(|e| eyre::eyre!("failed to read {}: {}", file.display(), e))?;
            let ops: Vec<BatchOp> = serde_json::from_str(&json)?;
            if ops.is_empty() {
                bail!("batch file contains no operations");
            }

            // Precompute $N -> entityKey for every CREATE in the batch, before
            // we send execute() (which would mutate the sender's nonce).
            let signer_nonce: u64 = registry.entityNonce(signer_address).call().await?;
            let chain_id = provider.get_chain_id().await?;
            let mut refs: HashMap<usize, B256> = HashMap::new();
            let mut create_count: u32 = 0;
            for (i, op) in ops.iter().enumerate() {
                if matches!(op, BatchOp::Create { .. }) {
                    let salt = match op {
                        BatchOp::Create { salt, .. } => *salt,
                        _ => 0,
                    };
                    let k = predict_entity_key(
                        chain_id,
                        cli.registry,
                        signer_address,
                        signer_nonce.saturating_add(create_count as u64),
                        salt,
                    );
                    refs.insert(i, k);
                    create_count += 1;
                }
            }

            let resolve = |r: &EntityKeyRef| -> Result<B256> {
                match r {
                    EntityKeyRef::Literal(k) => Ok(*k),
                    EntityKeyRef::Ref(i) => refs.get(i).copied().ok_or_else(|| {
                        eyre::eyre!("${} does not refer to a CREATE op in this batch", i)
                    }),
                }
            };

            let mut sol_ops: Vec<Operation> = Vec::with_capacity(ops.len());
            for op in &ops {
                let sol_op = match op {
                    BatchOp::Create {
                        content_type,
                        payload,
                        size,
                        expires_at,
                        min_lifetime,
                        salt,
                        creation_flags,
                        attributes,
                    } => {
                        let mut attrs = build_attributes(attributes, &resolve)?;
                        attrs.push(content_type_attr(content_type)?);
                        attrs.push(payload_attr(&resolve_payload(payload.as_deref(), *size)?)?);
                        Operation::create(*salt, *expires_at, *min_lifetime, *creation_flags, attrs)
                    }
                    BatchOp::Patch {
                        entity_key,
                        content_type,
                        payload,
                        size,
                        attributes,
                        unset,
                    } => {
                        let mut mutations = build_attributes(attributes, &resolve)?;
                        for name in unset {
                            mutations.push(Attribute::tombstone(Ident32::encode(name)?));
                        }
                        if let Some(ct) = content_type.as_deref() {
                            mutations.push(content_type_attr(ct)?);
                        }
                        if payload.is_some() || size.is_some() {
                            mutations
                                .push(payload_attr(&resolve_payload(payload.as_deref(), *size)?)?);
                        }
                        Operation::patch(resolve(entity_key)?, mutations)
                    }
                    BatchOp::ExtendExpiry {
                        entity_key,
                        expires_at,
                        min_lifetime,
                    } => Operation::extend_expiry(resolve(entity_key)?, *expires_at, *min_lifetime),
                    BatchOp::Transfer {
                        entity_key,
                        new_owner,
                    } => Operation::transfer_ownership(resolve(entity_key)?, *new_owner),
                    BatchOp::Delete { entity_key } => Operation::delete(resolve(entity_key)?),
                };
                sol_ops.push(sol_op);
            }

            let receipt = registry
                .execute(sol_ops)
                .gas_price(cli.gas_price)
                .send()
                .await?
                .get_receipt()
                .await?;
            println!("tx: {}", receipt.transaction_hash);
            print_events(receipt.inner.logs());
        }

        Command::Spam {
            count,
            size,
            min_lifetime,
        } => {
            let nonce_start = provider.get_transaction_count(signer_address).await?;

            // Fire all transactions, retrying on pool-full errors
            let mut pending = Vec::new();
            for i in 0..count {
                let nonce = nonce_start + i as u64;
                loop {
                    let op = Operation::create(
                        0,
                        0,
                        min_lifetime,
                        0,
                        vec![
                            content_type_attr("application/octet-stream")?,
                            payload_attr(&random_payload(size))?,
                        ],
                    );

                    match registry
                        .execute(vec![op])
                        .nonce(nonce)
                        .gas_price(cli.gas_price)
                        .send()
                        .await
                    {
                        Ok(p) => {
                            pending.push(p);
                            eprint!("\rsent {}/{}", i + 1, count);
                            break;
                        }
                        Err(e) if e.to_string().contains("txpool is full") => {
                            // Pool is full — wait for a block to drain it
                            tokio::time::sleep(cli.block_time).await;
                        }
                        Err(e) => {
                            eprintln!("\rsend failed at {}/{}: {}", i + 1, count, e);
                            break;
                        }
                    }
                }
            }
            eprintln!();

            // Wait for all receipts
            let mut success = 0u32;
            let mut failed = 0u32;
            let total = pending.len();
            for (i, p) in pending.into_iter().enumerate() {
                match p.get_receipt().await {
                    Ok(_) => success += 1,
                    Err(_) => failed += 1,
                }
                eprint!("\rconfirmed {}/{}", i + 1, total);
            }
            eprintln!();
            println!("{} ok, {} failed", success, failed);
        }

        Command::Balance { address } => {
            let addr = address.unwrap_or(signer_address);
            let balance = provider.get_balance(addr).await?;
            let eth = balance / U256::from(10u64).pow(U256::from(18));
            let remainder = balance % U256::from(10u64).pow(U256::from(18));
            println!("{addr}");
            println!("{eth}.{remainder:018} ETH");
        }
    }

    Ok(())
}
