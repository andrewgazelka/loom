//! `export` and `import`: a definition closure travels as one CARv1 bundle and
//! is rebuilt from source on the receiving node (format: docs/bundles.md).
//! Compiled wasm never travels; a rebuilt hash that differs from the recorded
//! one refuses the whole bundle, which is how toolchain drift surfaces.
use super::*;
use crate::definitions::Destination;
use crate::evolution::Publication;
use loom_check::SourceFile;
use loom_proto::bundle::{Frame, encode_car};
use loom_proto::{DAG_CBOR_CODEC, RAW_CODEC, cid_for_hash, parse_reference};
use loom_store::{Block, ImportBlock, MAX_BUNDLE_BYTES, verify_bundle};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// `loom_bundle` in the root block and `loom_definition` in every record.
pub const BUNDLE_VERSION: u64 = 1;
const MAX_OBJECTS: usize = 1_000_000;
const MAX_DEFINITIONS: usize = 1024;

/// Root block: which names the bundle binds, one record per definition in the
/// closure, and every CAS object the rebuild needs with its stored kind.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Root {
    loom_bundle: u64,
    roots: BTreeMap<String, String>,
    definitions: BTreeMap<String, Value>,
    objects: Vec<RootObject>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootObject {
    cid: Value,
    kind: String,
}
/// One definition: everything `add` needs to rebuild it plus the identity the
/// rebuild must reproduce. Links are DAG-CBOR CID links to `objects`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    loom_definition: u64,
    hash: String,
    lang: Lang,
    names: Vec<String>,
    source: Value,
    deps: BTreeMap<String, String>,
    allowed_effects: Option<Vec<String>>,
    sig: loom_proto::TypeSig,
    identity: Option<RecordIdentity>,
    preimages: Vec<Value>,
    trees: Vec<Value>,
    preparation: Option<RecordPreparation>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordIdentity {
    behavior_hash: String,
    wasm_hash: String,
    toolchain_hash: String,
    item_hashes: Value,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordPreparation {
    key: String,
    overlay: Value,
}
#[derive(Deserialize)]
struct ItemDocument {
    items: BTreeMap<String, ItemEntry>,
}
#[derive(Deserialize)]
struct ItemEntry {
    hash: String,
    #[serde(default)]
    cycle: Option<Vec<String>>,
}

fn is_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
fn link_cid(link: &Value) -> Result<&str> {
    let object = link.as_object().context("bundle link must be a CID link")?;
    ensure!(object.len() == 1, "bundle link must be a CID link");
    object
        .get("$ref")
        .and_then(Value::as_str)
        .context("bundle link must be a CID link")
}
/// Every CID link inside a decoded DAG-CBOR value.
fn links(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            if object.len() == 1
                && let Some(cid) = object.get("$ref").and_then(Value::as_str)
            {
                out.push(cid.to_owned());
                return;
            }
            for child in object.values() {
                links(child, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                links(child, out);
            }
        }
        _ => {}
    }
}

struct Exporter<'a> {
    store: &'a Store,
    /// CID -> (kind, bytes); insertion is idempotent.
    objects: BTreeMap<String, (String, Vec<u8>)>,
    bytes: usize,
}
impl Exporter<'_> {
    fn include(&mut self, hash: &str) -> Result<Value> {
        ensure!(is_hash(hash), "CAS reference {hash:?} is not a BLAKE3 hash");
        let entry = self
            .store
            .cas_entry(hash)?
            .with_context(|| format!("CAS object {hash} missing"))?;
        let codec = self
            .store
            .codec(hash)?
            .with_context(|| format!("CAS object {hash} has no codec"))?;
        let cid = cid_for_hash(hash, codec).map_err(anyhow::Error::msg)?;
        if !self.objects.contains_key(&cid) {
            let bytes = self
                .store
                .get(hash)?
                .with_context(|| format!("CAS object {hash} disappeared"))?;
            self.bytes = self
                .bytes
                .checked_add(bytes.len())
                .context("bundle size overflow")?;
            ensure!(
                self.bytes <= MAX_BUNDLE_BYTES,
                "bundle exceeds {MAX_BUNDLE_BYTES} bytes"
            );
            ensure!(
                self.objects.len() < MAX_OBJECTS,
                "bundle exceeds {MAX_OBJECTS} objects"
            );
            self.objects.insert(cid.clone(), (entry.kind, bytes));
        }
        Ok(json!({"$ref": cid}))
    }
    /// Include a DAG root (a crate tree, a resolver overlay) and every object it links.
    fn include_closure(&mut self, hash: &str) -> Result<Value> {
        let root = self.include(hash)?;
        let mut pending = vec![hash.to_owned()];
        let mut visited = BTreeSet::new();
        while let Some(hash) = pending.pop() {
            if !visited.insert(hash.clone()) {
                continue;
            }
            let link = self.include(&hash)?;
            let cid = link_cid(&link)?;
            let address = parse_reference(cid).map_err(anyhow::Error::msg)?;
            if address.codec != DAG_CBOR_CODEC {
                continue;
            }
            let value: Value =
                loom_proto::decode(&self.objects[cid].1).map_err(anyhow::Error::msg)?;
            let mut children = Vec::new();
            links(&value, &mut children);
            for child in children {
                pending.push(parse_reference(&child).map_err(anyhow::Error::msg)?.hash);
            }
        }
        Ok(root)
    }
}

impl Service {
    /// Resolve `targets` to `name -> current hash` roots. A hash is accepted
    /// only when it is the current value of at least one name: importing binds
    /// names, and a superseded revision has no name to bind.
    fn export_roots(&self, targets: &[String]) -> Result<BTreeMap<String, String>> {
        let names = self.store.current_names()?;
        let mut roots = BTreeMap::new();
        for target in targets {
            if let Some(hash) = names.get(target) {
                roots.insert(target.clone(), hash.clone());
                continue;
            }
            let hash = target.strip_prefix('#').unwrap_or(target);
            ensure!(
                self.store.definition(hash)?.is_some(),
                "export target {target:?} not found"
            );
            let bound: Vec<_> = names.iter().filter(|entry| entry.1 == hash).collect();
            ensure!(
                !bound.is_empty(),
                "export target {target:?} is a definition hash without a current name; export a name so import can bind it"
            );
            for entry in bound {
                roots.insert(entry.0.clone(), entry.1.clone());
            }
        }
        Ok(roots)
    }

    fn export_record(&self, exporter: &mut Exporter<'_>, hash: &str) -> Result<Record> {
        let store = &self.store;
        let def = store
            .definition(hash)?
            .with_context(|| format!("definition {hash} not found"))?;
        let source = store
            .source(hash)?
            .with_context(|| format!("definition {hash}: source missing"))?;
        let source_hash = store
            .definition_source_hash(hash)?
            .with_context(|| format!("definition {hash}: source hash missing"))?;
        let deps = store.definition_deps(hash)?;
        let names = store
            .current_names()?
            .into_iter()
            .filter(|entry| entry.1 == hash)
            .map(|entry| entry.0)
            .collect();
        let source_link = exporter.include(&source_hash)?;
        let mut preimages = BTreeSet::new();
        let mut trees = BTreeSet::new();
        let mut identity = None;
        let mut preparation = None;
        if !def.lang.is_v8() {
            let built = store
                .build_identity(hash)?
                .with_context(|| format!("definition {hash} has no compiler item identity"))?;
            let item_hashes = exporter.include(&built.item_hashes_ref)?;
            let document: ItemDocument = serde_json::from_slice(
                &store
                    .get(&built.item_hashes_ref)?
                    .context("item hash document missing from CAS")?,
            )?;
            for item in document.items.values() {
                if store.cas_entry(&item.hash)?.is_none() {
                    continue;
                }
                preimages.insert(link_cid(&exporter.include(&item.hash)?)?.to_owned());
                if item.cycle.is_some()
                    && let Some(data) = store.get(&item.hash)?
                    && data.len() == 40
                {
                    let cycle: String = data[..32]
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect();
                    if store.cas_entry(&cycle)?.is_some() {
                        preimages.insert(link_cid(&exporter.include(&cycle)?)?.to_owned());
                    }
                }
            }
            if store.cas_entry(hash)?.is_some() {
                preimages.insert(link_cid(&exporter.include(hash)?)?.to_owned());
            }
            identity = Some(RecordIdentity {
                behavior_hash: built.behavior_hash,
                wasm_hash: built.wasm_hash,
                toolchain_hash: built.toolchain_hash,
                item_hashes,
            });
            if source.trim_start().starts_with('{') {
                let bundle: loom_check::SourceBundle = serde_json::from_str(&source)?;
                if let Some(manifest) = bundle.files.get("Cargo.toml").and_then(SourceFile::as_text)
                {
                    for dependency in loom_check::crate_dependencies(manifest)
                        .map_err(anyhow::Error::msg)?
                        .into_values()
                    {
                        trees.insert(
                            link_cid(&exporter.include_closure(&dependency.hash)?)?.to_owned(),
                        );
                    }
                }
                if let Some(tree) = bundle
                    .files
                    .get(loom_build::VENDOR_TREE)
                    .and_then(SourceFile::as_text)
                {
                    trees.insert(link_cid(&exporter.include_closure(tree)?)?.to_owned());
                }
            }
            let checked = stored_definition(store, hash)?;
            let closure = dependency_closure(store, &checked.deps)?;
            if let Some(found) = self.builder.preparation(&checked, &closure)? {
                let overlay = exporter.include(&found.overlay_hash)?;
                let files: BTreeMap<String, SourceFile> = store
                    .get_value(&found.overlay_hash)?
                    .context("preparation overlay missing from CAS")?;
                if let Some(tree) = files
                    .get(loom_build::VENDOR_TREE)
                    .and_then(SourceFile::as_text)
                {
                    trees.insert(link_cid(&exporter.include_closure(tree)?)?.to_owned());
                }
                preparation = Some(RecordPreparation {
                    key: found.key,
                    overlay,
                });
            }
        }
        Ok(Record {
            loom_definition: BUNDLE_VERSION,
            hash: hash.to_owned(),
            lang: def.lang,
            names,
            source: source_link,
            deps,
            allowed_effects: def.allowed_effects,
            sig: def.sig,
            identity,
            preimages: preimages
                .into_iter()
                .map(|cid| json!({"$ref": cid}))
                .collect(),
            trees: trees.into_iter().map(|cid| json!({"$ref": cid})).collect(),
            preparation,
        })
    }

    /// Build the bundle for `targets`, store its bytes as a raw CAS object and
    /// return the reference clients download from `GET /v1/cas/{cid}`.
    pub(super) fn export_bundle(&self, args: &Value) -> Result<Value> {
        let targets: Vec<String> = serde_json::from_value(args["targets"].clone())?;
        let roots = self.export_roots(&targets)?;
        let mut pending: Vec<String> = roots.values().cloned().collect();
        let mut exporter = Exporter {
            store: &self.store,
            objects: BTreeMap::new(),
            bytes: 0,
        };
        let mut records: BTreeMap<String, Record> = BTreeMap::new();
        while let Some(hash) = pending.pop() {
            if records.contains_key(&hash) {
                continue;
            }
            ensure!(
                records.len() < MAX_DEFINITIONS,
                "bundle closure exceeds {MAX_DEFINITIONS} definitions"
            );
            let record = self.export_record(&mut exporter, &hash)?;
            pending.extend(record.deps.values().cloned());
            records.insert(hash, record);
        }
        let mut frames = Vec::new();
        let mut definitions = BTreeMap::new();
        for (hash, record) in &records {
            let bytes = loom_proto::encode(record).map_err(anyhow::Error::msg)?;
            let cid = cid_for_hash(&loom_store::content_hash(&bytes), DAG_CBOR_CODEC)
                .map_err(anyhow::Error::msg)?;
            definitions.insert(hash.clone(), json!({"$ref": cid}));
            frames.push(Frame { cid, bytes });
        }
        let root = Root {
            loom_bundle: BUNDLE_VERSION,
            roots: roots.clone(),
            definitions,
            objects: exporter
                .objects
                .iter()
                .map(|entry| RootObject {
                    cid: json!({"$ref": entry.0}),
                    kind: entry.1.0.clone(),
                })
                .collect(),
        };
        let root_bytes = loom_proto::encode(&root).map_err(anyhow::Error::msg)?;
        let root_cid = cid_for_hash(&loom_store::content_hash(&root_bytes), DAG_CBOR_CODEC)
            .map_err(anyhow::Error::msg)?;
        frames.insert(
            0,
            Frame {
                cid: root_cid.clone(),
                bytes: root_bytes,
            },
        );
        for (cid, (_, bytes)) in exporter.objects {
            frames.push(Frame { cid, bytes });
        }
        let bytes = encode_car(&root_cid, &frames).map_err(anyhow::Error::msg)?;
        let object_count = frames.len() - 1 - records.len();
        let hash = self.store.put("bundle", &bytes)?;
        Ok(json!({
            "bundle": self.store.reference(&hash, RAW_CODEC)?,
            "root": root_cid,
            "roots": roots,
            "definitions": records.len(),
            "objects": object_count,
            "bytes": bytes.len(),
        }))
    }
}

/// A verified bundle whose root and records have been decoded and cross-checked.
struct Plan {
    root_cid: String,
    roots: BTreeMap<String, String>,
    records: BTreeMap<String, Record>,
    /// Object CID -> (block, kind): the `objects` list of the root, every one
    /// of them reached from a record (directly or through a tree).
    objects: BTreeMap<String, (Block, String)>,
    /// Definition hashes, dependencies first.
    order: Vec<String>,
}

/// Decode and cross-check a bundle without touching any store. Every check a
/// bundle can fail on its own bytes lives here; the store-dependent checks
/// (name conflicts, the preparation key, the rebuilt hash) come later.
fn plan(bytes: &[u8]) -> Result<Plan> {
    let verified = verify_bundle(bytes)?;
    let mut blocks = verified.blocks.into_iter();
    let root_block = blocks.next().context("bundle has no root block")?;
    let root: Root = loom_proto::decode(&root_block.bytes)
        .map_err(anyhow::Error::msg)
        .context("bundle root is not a Loom bundle root")?;
    ensure!(
        root.loom_bundle == BUNDLE_VERSION,
        "bundle format {} is not supported; this node reads loom_bundle {BUNDLE_VERSION}",
        root.loom_bundle
    );
    // The caps come before any per-entry work so a root listing a hundred
    // thousand definitions costs one comparison, not a hundred thousand decodes.
    ensure!(
        root.definitions.len() <= MAX_DEFINITIONS,
        "bundle lists {} definitions; this node imports at most {MAX_DEFINITIONS}",
        root.definitions.len()
    );
    ensure!(
        root.objects.len() <= MAX_OBJECTS,
        "bundle lists {} objects; this node imports at most {MAX_OBJECTS}",
        root.objects.len()
    );
    let mut by_cid: BTreeMap<String, Block> =
        blocks.map(|block| (block.cid.clone(), block)).collect();
    let mut objects = BTreeMap::new();
    for object in root.objects {
        let cid = link_cid(&object.cid)?;
        let block = by_cid.remove(cid).with_context(|| {
            format!(
                "bundle root lists object {cid} but the bundle has no such block, or lists it twice"
            )
        })?;
        objects.insert(cid.to_owned(), (block, object.kind));
    }
    let mut records = BTreeMap::new();
    for (hash, link) in &root.definitions {
        ensure!(
            is_hash(hash),
            "bundle definition key {hash:?} is not a hash"
        );
        let cid = link_cid(link)?;
        let block = by_cid
            .remove(cid)
            .with_context(|| format!("bundle definition {hash} record {cid} is missing, listed twice, or listed as an object"))?;
        let record: Record = loom_proto::decode(&block.bytes)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("bundle definition {hash} record {cid} is malformed"))?;
        ensure!(
            record.loom_definition == BUNDLE_VERSION && record.hash == *hash,
            "bundle definition record {cid} does not describe {hash}"
        );
        ensure!(
            record.identity.is_some() != record.lang.is_v8(),
            "bundle definition {hash} is {} but {} a compiler identity",
            record.lang.as_str(),
            if record.identity.is_some() {
                "carries"
            } else {
                "lacks"
            }
        );
        for dependency in record.deps.values() {
            ensure!(
                root.definitions.contains_key(dependency),
                "bundle lacks definition {dependency} required by {hash}"
            );
        }
        records.insert(hash.clone(), record);
    }
    if let Some(stray) = by_cid.keys().next() {
        bail!("bundle block {stray} is not referenced by the root");
    }
    let referenced = referenced_objects(&records, &objects)?;
    if let Some(stray) = objects.keys().find(|cid| !referenced.contains(*cid)) {
        bail!("bundle root lists object {stray} which no definition record or tree references");
    }
    for (hash, record) in &records {
        check_preparation(hash, record, &objects)?;
    }
    for (name, hash) in &root.roots {
        validate_name(name).with_context(|| format!("bundle root name {name:?}"))?;
        ensure!(
            records.contains_key(hash),
            "bundle root {name:?} names definition {hash} which the bundle lacks"
        );
    }
    let order = dependency_order(&records)?;
    Ok(Plan {
        root_cid: verified.root,
        roots: root.roots,
        records,
        objects,
        order,
    })
}

/// Every object some record reaches: its direct links (source, item document,
/// preimages, trees, overlay), then every link inside a reached DAG-CBOR object
/// (subtrees, blobs), to closure. A link to an unlisted object is an error
/// naming the record or object that carries it. Iterative: a deep tree cannot
/// exhaust the stack.
fn referenced_objects(
    records: &BTreeMap<String, Record>,
    objects: &BTreeMap<String, (Block, String)>,
) -> Result<BTreeSet<String>> {
    let mut referenced = BTreeSet::new();
    let mut pending: Vec<(String, String)> = Vec::new();
    for (hash, record) in records {
        let mut direct = Vec::new();
        links(&serde_json::to_value(record)?, &mut direct);
        pending.extend(
            direct
                .into_iter()
                .map(|cid| (cid, format!("definition {hash}"))),
        );
    }
    while let Some((cid, owner)) = pending.pop() {
        let (block, _) = objects.get(&cid).with_context(|| {
            format!("bundle {owner} links object {cid} which the root does not list")
        })?;
        if !referenced.insert(cid.clone()) {
            continue;
        }
        if block.codec == DAG_CBOR_CODEC {
            let value: Value = loom_proto::decode(&block.bytes).map_err(anyhow::Error::msg)?;
            let mut children = Vec::new();
            links(&value, &mut children);
            pending.extend(
                children
                    .into_iter()
                    .map(|child| (child, format!("object {cid}"))),
            );
        }
    }
    Ok(referenced)
}

/// A record's resolver overlay must be what `preparation::save` writes: a
/// DAG-CBOR map holding only `Cargo.lock` and `loom.vendor-tree`, and the
/// vendor tree it names must travel in the record's `trees`, so the receiver
/// materializes the crates the overlay points at from hashed bundle bytes,
/// never from anywhere else. The key itself is checked against this node's
/// derivation at import time, once the record's dependencies are staged.
fn check_preparation(
    hash: &str,
    record: &Record,
    objects: &BTreeMap<String, (Block, String)>,
) -> Result<()> {
    let Some(preparation) = &record.preparation else {
        return Ok(());
    };
    ensure!(
        record.identity.is_some(),
        "bundle definition {hash} carries a resolver preparation but no compiler identity"
    );
    ensure!(
        is_hash(&preparation.key),
        "bundle definition {hash} preparation key {:?} is not a hash",
        preparation.key
    );
    let cid = link_cid(&preparation.overlay)?;
    let (block, _) = objects.get(cid).with_context(|| {
        format!("bundle definition {hash} preparation overlay {cid} is not listed")
    })?;
    ensure!(
        block.codec == DAG_CBOR_CODEC,
        "bundle definition {hash} preparation overlay {cid} is not DAG-CBOR"
    );
    let overlay: BTreeMap<String, SourceFile> = loom_proto::decode(&block.bytes)
        .map_err(anyhow::Error::msg)
        .with_context(|| {
            format!("bundle definition {hash} preparation overlay {cid} is malformed")
        })?;
    if let Some(unexpected) = overlay
        .keys()
        .find(|key| !["Cargo.lock", loom_build::VENDOR_TREE].contains(&key.as_str()))
    {
        bail!(
            "bundle definition {hash} preparation overlay {cid} carries {unexpected:?}; an overlay holds only Cargo.lock and {}",
            loom_build::VENDOR_TREE
        );
    }
    if let Some(tree) = overlay.get(loom_build::VENDOR_TREE) {
        let tree = tree.as_text().with_context(|| {
            format!("bundle definition {hash} preparation overlay names a non-text vendor tree")
        })?;
        ensure!(
            is_hash(tree),
            "bundle definition {hash} preparation overlay vendor tree {tree:?} is not a hash"
        );
        let tree_cid = cid_for_hash(tree, DAG_CBOR_CODEC).map_err(anyhow::Error::msg)?;
        let trees = record
            .trees
            .iter()
            .map(link_cid)
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            trees.contains(&tree_cid.as_str()),
            "bundle definition {hash} preparation overlay points at vendor tree {tree_cid} which the record's trees do not include"
        );
    }
    Ok(())
}

/// Definition hashes with every dependency before its dependents. Kahn's
/// algorithm over the `deps` edges: no recursion, so a chain of any length
/// costs stack for one frame; anything left unordered lies on a cycle.
/// Every `deps` value is a key of `records` (checked by `plan`).
fn dependency_order(records: &BTreeMap<String, Record>) -> Result<Vec<String>> {
    let mut blocked: BTreeMap<&str, usize> = BTreeMap::new();
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (hash, record) in records {
        let deps: BTreeSet<&str> = record.deps.values().map(String::as_str).collect();
        blocked.insert(hash, deps.len());
        for dep in deps {
            dependents.entry(dep).or_default().push(hash);
        }
    }
    let mut ready: BTreeSet<&str> = blocked
        .iter()
        .filter(|entry| *entry.1 == 0)
        .map(|entry| *entry.0)
        .collect();
    let mut order = Vec::with_capacity(records.len());
    while let Some(hash) = ready.pop_first() {
        order.push(hash.to_owned());
        for dependent in dependents.get(hash).into_iter().flatten().copied() {
            let remaining = blocked.get_mut(dependent).with_context(|| {
                format!("bundle definition {dependent} vanished while ordering")
            })?;
            *remaining -= 1;
            if *remaining == 0 {
                ready.insert(dependent);
            }
        }
    }
    if order.len() != records.len() {
        let stuck = blocked
            .iter()
            .find(|entry| *entry.1 > 0)
            .map(|entry| *entry.0)
            .context("bundle dependency cycle without a member")?;
        bail!("bundle dependency cycle through {stuck}");
    }
    Ok(order)
}

/// The one rule for a name a bundle may bind: every root name and the `into`
/// prefix pass through here. A name is resolved by `Store::resolve`, which
/// strips a leading `#` and looks a 64-hex string up as a definition hash
/// first, so either shape would bind a name nothing can resolve.
pub(super) fn validate_name(name: &str) -> Result<()> {
    ensure!(!name.is_empty(), "definition name is empty");
    ensure!(
        !name.chars().any(|ch| ch.is_whitespace() || ch.is_control()),
        "definition name {name:?} contains whitespace or control characters"
    );
    ensure!(
        !name.starts_with('#'),
        "definition name {name:?} starts with '#', which resolves as a hash reference"
    );
    ensure!(
        !is_hash(name),
        "definition name {name:?} is shaped like a definition hash"
    );
    ensure!(
        !name.starts_with('/') && !name.ends_with('/') && !name.contains("//"),
        "definition name {name:?} has an empty path segment"
    );
    Ok(())
}

/// `into` becomes a `prefix/` applied to every bound name. A valid prefix
/// joined to a valid root name is itself valid: the prefix ends in one `/`,
/// the name starts with neither `/` nor `#`, and the join is never 64 hex.
fn import_prefix(into: Option<&str>) -> Result<String> {
    let Some(into) = into else {
        return Ok(String::new());
    };
    let bare = into.strip_suffix('/').unwrap_or(into);
    validate_name(bare).with_context(|| format!("import prefix {into:?}"))?;
    Ok(format!("{bare}/"))
}

impl Service {
    /// Seed the staged resolver cache with a record's overlay, on the staged
    /// service (`self.store` is the private snapshot). The bundle's
    /// `preparation.key` is a claim about this node's own derivation, and the
    /// row lands under it only once the claim holds: the overlay is staged
    /// under the claimed key, then the builder is asked which row it reads for
    /// the rebuilt record. Any other answer refuses the import, so a bundle
    /// cannot plant an overlay under a key this node computes for unrelated
    /// definitions; the staged store, poisoned row included, is discarded with
    /// the refusal. A key this node already resolved keeps its own overlay.
    /// Runs after the record's dependencies are staged: the derivation reads
    /// their stored sources.
    fn seed_preparation(&self, record: &Record, source: &str, label: &str) -> Result<()> {
        let Some(preparation) = &record.preparation else {
            return Ok(());
        };
        let overlay = parse_reference(link_cid(&preparation.overlay)?)
            .map_err(anyhow::Error::msg)?
            .hash;
        let checked = loom_check::CheckedDef {
            hash: record.hash.clone(),
            lang: record.lang,
            name: label.to_owned(),
            source: source.to_owned(),
            deps: record.deps.clone(),
            sig: record.sig.clone(),
            diagnostics: Vec::new(),
        };
        let closure = dependency_closure(&self.store, &record.deps)?;
        // Derive the key locally BEFORE anything is seeded: the bundle's key is
        // a claim, and only a row under the key this node computes for this
        // exact source may exist afterwards.
        match self.builder.preparation_key(&checked, &closure)? {
            Some(derived) if derived == preparation.key => {
                self.store.set_preparation(&derived, &overlay)?;
                Ok(())
            }
            Some(derived) => bail!(
                "import refused: definition {} bundles preparation key {} but this node derives {} for it; the bundle would seed a resolver row this node never computes for that source",
                record.hash,
                preparation.key,
                derived
            ),
            None => bail!(
                "import refused: definition {} bundles preparation key {}, but this node derives no resolver key for it (it carries its own vendor tree)",
                record.hash,
                preparation.key
            ),
        }
    }

    /// Import a bundle previously stored with `POST /v1/cas`. `plan` checks
    /// the bytes; under the definitions gate every definition is rebuilt
    /// through the `add` admission path into a private staged store, its
    /// resolver row seeded only under the key this node derives, and the whole
    /// graph is published in one transaction; nothing reaches the live store
    /// unless every rebuilt hash equals its recorded hash.
    pub(super) async fn import_bundle(&self, args: &Value) -> Result<Value> {
        let reference = field(args, "bundle")?;
        let hash = if is_hash(reference) {
            reference.to_owned()
        } else {
            let address = parse_reference(reference.strip_prefix('#').unwrap_or(reference))
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("bundle reference {reference:?}"))?;
            ensure!(
                address.codec == RAW_CODEC,
                "bundle reference must name a raw CAS object"
            );
            address.hash
        };
        let bytes = self.store.get(&hash)?.with_context(|| {
            format!("bundle {reference} not found in the CAS; upload it with POST /v1/cas first")
        })?;
        let prefix = import_prefix(
            args.get("into")
                .filter(|value| !value.is_null())
                .map(|value| value.as_str().context("into must be a string"))
                .transpose()?,
        )?;
        let planned = plan(&bytes)?;
        let _guard = self.definitions_gate.lock().await;
        let expected_names = self.store.current_names()?;
        let mut bound: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (name, hash) in &planned.roots {
            let target = format!("{prefix}{name}");
            if let Some(existing) = expected_names.get(&target) {
                ensure!(
                    existing == hash,
                    "import conflict: name {target:?} is bound to {existing} but the bundle binds it to {hash}; import with a different --into prefix or update the existing name"
                );
            }
            bound.entry(hash.clone()).or_default().push(target);
        }
        let mut staged = self.clone();
        staged.store = self.store.stage_intake()?;
        staged.builder = Arc::new(self.builder.for_store(staged.store.clone()));
        // `plan` refused any listed object no record reaches, so this is the
        // referenced set, nothing more.
        let blocks: Vec<ImportBlock<'_>> = planned
            .objects
            .values()
            .map(|entry| ImportBlock {
                block: &entry.0,
                kind: entry.1.as_str(),
            })
            .collect();
        staged.store.import_blocks(&blocks)?;
        let mut publications = Vec::new();
        for hash in &planned.order {
            let record = &planned.records[hash];
            let names = bound.get(hash).cloned().unwrap_or_default();
            let label = names.first().cloned().unwrap_or_else(|| hash.clone());
            let source_cid = link_cid(&record.source)?;
            let source = String::from_utf8(planned.objects[source_cid].0.bytes.clone())
                .with_context(|| format!("import of {label}: source is not UTF-8"))?;
            staged.seed_preparation(record, &source, &label)?;
            let response = staged
                .admit(
                    DefineRequest {
                        lang: record.lang,
                        // A private name never reaches the live namespace.
                        name: names
                            .first()
                            .cloned()
                            .unwrap_or_else(|| format!("__import_{hash}")),
                        source: source.clone(),
                        deps: record.deps.clone(),
                        allowed_effects: record.allowed_effects.clone(),
                    },
                    Destination::Staged,
                )
                .await?;
            ensure!(
                response.ok,
                "import of {label}: rebuild failed: {}",
                serde_json::to_string(
                    &json!({"diagnostics": response.diagnostics, "build": response.result})
                )?
            );
            let rebuilt = response.result["def"]["hash"]
                .as_str()
                .context("rebuilt definition hash missing")?
                .to_owned();
            if rebuilt != *hash {
                let recorded = record
                    .identity
                    .as_ref()
                    .map(|identity| identity.toolchain_hash.clone())
                    .unwrap_or_else(|| "none (script identity)".into());
                let local = staged
                    .store
                    .build_identity(&rebuilt)?
                    .map(|identity| identity.toolchain_hash)
                    .unwrap_or_else(|| "none (script identity)".into());
                bail!(
                    "import refused: rebuilding {label} produced definition {rebuilt} but the bundle records {hash}; bundle toolchain {recorded}, this node's toolchain {local}"
                );
            }
            let definition = staged
                .store
                .resolve(hash)?
                .context("rebuilt definition missing")?;
            let mut event = response.result["build"].clone();
            event["type"] = json!("component_built");
            publications.push(Publication {
                identity: if definition.lang.is_v8() {
                    None
                } else {
                    Some(
                        staged
                            .store
                            .build_identity(hash)?
                            .context("rebuilt identity missing")?,
                    )
                },
                deps: staged.store.definition_deps(hash)?,
                definition,
                names,
                source,
                event,
            });
        }
        let mut intake = Vec::new();
        for publication in &publications {
            let names: Vec<Option<&str>> = if publication.names.is_empty() {
                vec![None]
            } else {
                publication
                    .names
                    .iter()
                    .map(|name| Some(name.as_str()))
                    .collect()
            };
            for name in names {
                intake.push(loom_store::IntakePublication {
                    def: &publication.definition,
                    name,
                    source: &publication.source,
                    deps: &publication.deps,
                    identity: publication.identity.as_ref(),
                    build_event: &publication.event,
                });
            }
        }
        let seq = self
            .store
            .commit_import(&staged.store, &intake, &expected_names)?;
        let imported: Vec<Value> = planned
            .roots
            .iter()
            .map(|entry| json!({"name": format!("{prefix}{}", entry.0), "hash": entry.1}))
            .collect();
        Ok(json!({
            "root": planned.root_cid,
            "bundle": self.store.reference(&hash, RAW_CODEC)?,
            "prefix": prefix,
            "imported": imported,
            "definitions": planned.order.len(),
            "objects": planned.objects.len(),
            "seq": seq,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_end_with_one_slash_and_reject_empty_segments() {
        assert_eq!(import_prefix(None).unwrap(), "");
        assert_eq!(import_prefix(Some("friend")).unwrap(), "friend/");
        assert_eq!(import_prefix(Some("friend/")).unwrap(), "friend/");
        assert_eq!(import_prefix(Some("a/b")).unwrap(), "a/b/");
        for invalid in ["", "/", "/friend", "a//b", "a//", "a b", "a\tb", "#friend"] {
            assert!(import_prefix(Some(invalid)).is_err(), "{invalid:?}");
        }
        let hash = "ab".repeat(32);
        assert!(import_prefix(Some(hash.as_str())).is_err());
    }

    #[test]
    fn names_that_resolve_as_something_else_are_refused() {
        for valid in ["sum", "friend/sum", "a.b-c_d", "#inside#ok", "abc123"] {
            validate_name(valid).unwrap_or_else(|error| panic!("{valid:?}: {error}"));
        }
        let hash = "0f".repeat(32);
        for invalid in [
            "",
            "#sum",
            hash.as_str(),
            "/sum",
            "sum/",
            "a//b",
            "a b",
            "a\nb",
            "a\u{7}b",
        ] {
            let error = validate_name(invalid).unwrap_err().to_string();
            assert!(error.contains("definition name"), "{invalid:?}: {error}");
        }
        // The `#` refusal names the reason, since `#sum` looks like a plain name.
        assert!(
            validate_name("#sum")
                .unwrap_err()
                .to_string()
                .contains("'#'")
        );
    }

    fn record(hash: &str, deps: &[(&str, &str)]) -> Record {
        Record {
            loom_definition: BUNDLE_VERSION,
            hash: hash.to_owned(),
            lang: Lang::JavaScript,
            names: Vec::new(),
            source: json!({"$ref": "bafy"}),
            deps: deps
                .iter()
                .map(|entry| (entry.0.to_owned(), entry.1.to_owned()))
                .collect(),
            allowed_effects: None,
            sig: Default::default(),
            identity: None,
            preimages: Vec::new(),
            trees: Vec::new(),
            preparation: None,
        }
    }
    fn fake_hash(index: usize) -> String {
        format!("{index:064x}")
    }

    #[test]
    fn dependency_order_puts_dependencies_first_without_recursion() {
        // A 100,000-long chain: the recursive walk this replaced overflowed the
        // worker stack on chains far shorter than this.
        let length = 100_000;
        let mut records = BTreeMap::new();
        for index in 0..length {
            let deps: Vec<(String, String)> = if index == 0 {
                Vec::new()
            } else {
                vec![("prev".to_owned(), fake_hash(index - 1))]
            };
            let deps: Vec<(&str, &str)> = deps
                .iter()
                .map(|entry| (entry.0.as_str(), entry.1.as_str()))
                .collect();
            records.insert(fake_hash(index), record(&fake_hash(index), &deps));
        }
        let order = dependency_order(&records).unwrap();
        assert_eq!(order.len(), length);
        let position: BTreeMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|entry| (entry.1.as_str(), entry.0))
            .collect();
        for (hash, record) in &records {
            for dep in record.deps.values() {
                assert!(
                    position[dep.as_str()] < position[hash.as_str()],
                    "{dep} after {hash}"
                );
            }
        }
    }

    #[test]
    fn dependency_order_counts_a_dependency_once_per_hash_and_refuses_cycles() {
        // Two aliases of one dependency are one edge.
        let util = fake_hash(1);
        let caller = fake_hash(2);
        let records = BTreeMap::from([
            (util.clone(), record(&util, &[])),
            (
                caller.clone(),
                record(&caller, &[("a", util.as_str()), ("b", util.as_str())]),
            ),
        ]);
        assert_eq!(
            dependency_order(&records).unwrap(),
            vec![util.clone(), caller.clone()]
        );
        // Deterministic: independent roots come out in hash order.
        let lone = fake_hash(0);
        let mut records = records;
        records.insert(lone.clone(), record(&lone, &[]));
        assert_eq!(
            dependency_order(&records).unwrap(),
            vec![lone, util.clone(), caller.clone()]
        );
        // A two-cycle and a self-dependency are both refused, naming a member.
        let a = fake_hash(3);
        let b = fake_hash(4);
        let cyclic = BTreeMap::from([
            (a.clone(), record(&a, &[("b", b.as_str())])),
            (b.clone(), record(&b, &[("a", a.as_str())])),
        ]);
        let error = dependency_order(&cyclic).unwrap_err().to_string();
        assert!(
            error.contains("cycle") && (error.contains(&a) || error.contains(&b)),
            "{error}"
        );
        let selfish = BTreeMap::from([(a.clone(), record(&a, &[("me", a.as_str())]))]);
        let error = dependency_order(&selfish).unwrap_err().to_string();
        assert!(error.contains("cycle") && error.contains(&a), "{error}");
    }

    #[test]
    fn links_are_collected_from_nested_values_only_when_shaped_as_references() {
        let value = json!({
            "a": {"$ref": "bafy1"},
            "b": [{"$ref": "bafy2"}, {"$ref": "bafy3", "other": 1}],
            "c": {"nested": {"$ref": "bafy4"}},
        });
        let mut found = Vec::new();
        links(&value, &mut found);
        assert_eq!(found, vec!["bafy1", "bafy2", "bafy4"]);
        assert!(link_cid(&json!({"$ref": "bafy3", "other": 1})).is_err());
    }
}
