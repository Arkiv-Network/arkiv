mod simulate;

use alloy_network::EthereumWallet;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types::eth::Log as RpcLog;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolEvent;
use arkiv_bindings::{IEntityRegistry::EntityOperation, op_type_name, *};
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

        /// Blocks-to-live: how many blocks until the entity expires.
        #[arg(long)]
        btl: u32,

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

    /// Update an existing entity. Either `--payload` or `--random-payload` must be set.
    Update {
        /// Entity key to update.
        #[arg(long)]
        key: B256,

        /// Content type MIME string.
        #[arg(long, default_value = "application/octet-stream")]
        content_type: String,

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

    /// Extend an entity's expiration.
    Extend {
        /// Entity key to extend.
        #[arg(long)]
        key: B256,

        /// Blocks-to-live: how many blocks until the entity expires.
        #[arg(long)]
        btl: u32,
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

    /// Expire an entity (must be past its expiration block).
    Expire {
        /// Entity key to expire.
        #[arg(long)]
        key: B256,
    },

    /// Query an entity's on-chain commitment.
    Query {
        /// Entity key to query.
        #[arg(long)]
        key: B256,
    },

    /// Read the current changeset hash.
    Hash,

    /// Walk the changeset hash chain from head to genesis.
    History {
        /// Maximum number of operations to display (default: all).
        #[arg(long)]
        depth: Option<u32>,
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
    Spam {
        /// Number of entities to create.
        #[arg(long, default_value = "10")]
        count: u32,

        /// Payload size in bytes per entity.
        #[arg(long, default_value = "256")]
        size: usize,

        /// Blocks-to-live: how many blocks until each entity expires.
        #[arg(long)]
        btl: u32,
    },

    /// Continuously generate a weighted mix of entity operations against
    /// a running node, simulating live system traffic.
    Simulate(simulate::SimulateArgs),
}

fn random_payload(size: usize) -> Bytes {
    let mut rng = rand::rng();
    let mut buf = vec![0u8; size];
    rng.fill(&mut buf[..]);
    Bytes::from(buf)
}

fn print_events(logs: &[RpcLog]) {
    for log in logs {
        if let Ok(event) = EntityOperation::decode_log(&log.inner) {
            let e = event.data;
            println!("---");
            println!("  op:          {}", op_type_name(e.operationType));
            println!("  entity_key:  {}", e.entityKey);
            println!("  owner:       {}", e.owner);
            println!("  expires_at:  {}", e.expiresAt);
            println!("  entity_hash: {}", e.entityHash);
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
    Uint {
        uint: U256,
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
        btl: u32,
        #[serde(default)]
        attributes: Vec<BatchAttribute>,
    },
    Update {
        #[serde(rename = "entityKey")]
        entity_key: EntityKeyRef,
        #[serde(default = "default_content_type", rename = "contentType")]
        content_type: String,
        payload: Option<String>,
        size: Option<usize>,
        #[serde(default)]
        attributes: Vec<BatchAttribute>,
    },
    Extend {
        #[serde(rename = "entityKey")]
        entity_key: EntityKeyRef,
        btl: u32,
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
    Expire {
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
    Ok(match &attr.value {
        BatchAttributeValue::Uint { uint } => Attribute::uint(name, *uint),
        BatchAttributeValue::String { string } => Attribute::string(name, string.as_bytes())?,
        BatchAttributeValue::EntityKey { entity_key } => {
            Attribute::entity_key(name, resolve(entity_key)?)
        }
    })
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
        Some("uint" | "u256") => BatchAttributeValue::Uint {
            uint: parse_cli_uint_value(value)?,
        },
        Some("entityKey" | "entity-key" | "key") => BatchAttributeValue::EntityKey {
            entity_key: EntityKeyRef::Literal(parse_cli_entity_key_value(value)?),
        },
        Some(other) => bail!(
            "unknown attribute type '{}'; expected string, uint, or entityKey",
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
            btl,
            payload,
            random_payload,
            size,
            attributes,
        } => {
            let resolved_payload = resolve_cli_payload(payload.as_deref(), random_payload, size)?;
            let content_type = Mime128::encode(&content_type)?;
            let attributes = build_cli_attributes(&attributes, "create")?;
            let op = Operation::create(btl, resolved_payload, content_type, attributes);

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

        Command::Update {
            key,
            content_type,
            payload,
            random_payload,
            size,
            attributes,
        } => {
            let resolved_payload = resolve_cli_payload(payload.as_deref(), random_payload, size)?;
            let op = Operation::update(
                key,
                resolved_payload,
                Mime128::encode(&content_type)?,
                build_cli_attributes(&attributes, "update")?,
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

        Command::Extend { key, btl } => {
            let op = Operation::extend(key, btl);

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
            let op = Operation::transfer(key, new_owner);

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

        Command::Expire { key } => {
            let op = Operation::expire(key);

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

        Command::Query { key } => {
            let result = registry.commitment(key).call().await?;
            let c = result;
            println!("creator:    {}", c.creator);
            println!("owner:      {}", c.owner);
            println!("created_at: {}", c.createdAt);
            println!("updated_at: {}", c.updatedAt);
            println!("expires_at: {}", c.expiresAt);
            println!("core_hash:  {}", c.coreHash);
        }

        Command::Hash => {
            let hash = registry.changeSetHash().call().await?;
            println!("{hash}");
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

        Command::History { depth } => {
            let head = registry.headBlock().call().await?;
            let genesis = registry.genesisBlock().call().await?;

            if head == genesis {
                let node = registry.getBlockNode(head).call().await?;
                if node.txCount == 0 {
                    println!("No operations recorded.");
                    return Ok(());
                }
            }

            // Collect blocks from head back to genesis
            let mut block_num = head;
            let mut blocks = Vec::new();
            loop {
                let node = registry.getBlockNode(block_num).call().await?;
                let prev = node.prevBlock;
                if node.txCount > 0 {
                    blocks.push((block_num, node));
                }
                if block_num == genesis || prev == 0 {
                    break;
                }
                block_num = prev;
            }

            // Print chronologically, respecting depth limit on ops
            blocks.reverse();
            let max_ops = depth.unwrap_or(u32::MAX);
            let mut op_count_total: u32 = 0;

            'outer: for (block_num, node) in &blocks {
                println!("block {}", block_num);
                for tx_seq in 0..node.txCount {
                    let op_count = registry.txOpCount(*block_num, tx_seq).call().await?;
                    println!("  tx {}", tx_seq);
                    for op_seq in 0..op_count {
                        if op_count_total >= max_ops {
                            break 'outer;
                        }
                        let hash = registry
                            .changeSetHashAtOp(*block_num, tx_seq, op_seq)
                            .call()
                            .await?;
                        println!("    op {} -> {}", op_seq, hash);
                        op_count_total += 1;
                    }
                }
            }
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
            let signer_nonce: u32 = registry.nonces(signer_address).call().await?;
            let mut refs: HashMap<usize, B256> = HashMap::new();
            let mut create_count: u32 = 0;
            for (i, op) in ops.iter().enumerate() {
                if matches!(op, BatchOp::Create { .. }) {
                    let k = registry
                        .entityKey(signer_address, signer_nonce + create_count)
                        .call()
                        .await?;
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
                        btl,
                        attributes,
                    } => Operation::create(
                        *btl,
                        resolve_payload(payload.as_deref(), *size)?,
                        Mime128::encode(content_type)?,
                        build_attributes(attributes, &resolve)?,
                    ),
                    BatchOp::Update {
                        entity_key,
                        content_type,
                        payload,
                        size,
                        attributes,
                    } => Operation::update(
                        resolve(entity_key)?,
                        resolve_payload(payload.as_deref(), *size)?,
                        Mime128::encode(content_type)?,
                        build_attributes(attributes, &resolve)?,
                    ),
                    BatchOp::Extend { entity_key, btl } => {
                        Operation::extend(resolve(entity_key)?, *btl)
                    }
                    BatchOp::Transfer {
                        entity_key,
                        new_owner,
                    } => Operation::transfer(resolve(entity_key)?, *new_owner),
                    BatchOp::Delete { entity_key } => Operation::delete(resolve(entity_key)?),
                    BatchOp::Expire { entity_key } => Operation::expire(resolve(entity_key)?),
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

        Command::Spam { count, size, btl } => {
            let nonce_start = provider.get_transaction_count(signer_address).await?;

            // Fire all transactions, retrying on pool-full errors
            let mut pending = Vec::new();
            for i in 0..count {
                let nonce = nonce_start + i as u64;
                loop {
                    let op = Operation::create(
                        btl,
                        random_payload(size),
                        Mime128::encode("application/octet-stream")?,
                        vec![],
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
