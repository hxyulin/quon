//! Content fingerprint over the declared external-dep set.
//!
//! The fingerprint is the single signal `quon build` uses to decide
//! whether the current `vendor/` state is in sync with what the model
//! declares. If the fingerprint in `quon.lock` matches
//! [`fingerprint_external_deps`] applied to the live
//! `BuildModel::external_deps`, the vendor tree is considered fresh
//! and the synthetic `CrateDef`s in the lock are auto-registered into
//! the model. A mismatch forces the user to re-run `quon vendor`.
//!
//! # What participates in the hash
//!
//! Only fields that affect `cargo vendor`'s *output* participate:
//!
//! - name
//! - source (incl. git url + ref, crates-io version, path)
//! - features + default_features
//! - target (`Host` vs `Cross(<target-name>)`)
//!
//! `target` participates because the V2 cross pipeline runs
//! `cargo vendor` per target into a separate scratch workspace —
//! adding/removing `.target("kernel")` to a dep changes which workspace
//! the dep lives in, which is a vendor-output change.
//!
//! Fields that affect compilation but not vendoring — `cfg_flags`,
//! `rustc_flags` — are **deliberately excluded**. Changing a cfg flag
//! does not invalidate the vendored tree; the compile-time cache
//! (hashed over the full rustc invocation) already catches it.
//!
//! Likewise, the *spec* of a target (its triple, panic strategy, etc.)
//! is excluded: only the target's *name* is folded in. Changing the
//! triple of an existing target does not change which crates `cargo
//! vendor` pulls in our setup; the per-node compile stamps already
//! invalidate downstream rustc invocations.
//!
//! `span` is also excluded — source-position wobble is not a semantic
//! change.
//!
//! # Determinism
//!
//! Iteration is by name via `Arena::names()`, which walks a
//! `BTreeMap<String, _>` — already sorted. Within each entry,
//! `features` is sorted before hashing. The whole thing goes through
//! a domain-separated SHA-256 so future fingerprint schemes can
//! coexist with v1-format lockfiles if we ever need to migrate.

use quon_model::{Arena, DepSource, ExternalDepDef, GitRef, TargetDef, TargetSpec};
use sha2::{Digest, Sha256};

/// Domain tag. Bumping this invalidates every existing `quon.lock`
/// and forces a re-vendor on the next build. Only change it if the
/// hash algorithm itself changes; adding a new participating field
/// does not require a bump as long as it extends the encoding
/// append-only.
const DOMAIN: &str = "quon.vendor.fingerprint.v1";

/// Compute the fingerprint for a `BuildModel::external_deps` arena.
///
/// `targets` is needed to resolve each `TargetSpec::Cross(handle)` to
/// the target's stable name string. Arena indices are insertion-order
/// and not a durable identifier, so they cannot participate in the
/// hash directly.
///
/// Returns a string of the form `"sha256:<64 hex chars>"`. The empty
/// arena has a well-defined fingerprint (the SHA-256 of just the
/// domain tag + a zero length prefix) — it is never the empty string,
/// so `quon.lock::fingerprint` is always comparable.
pub fn fingerprint_external_deps(
    deps: &Arena<ExternalDepDef>,
    targets: &Arena<TargetDef>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN.as_bytes());
    hasher.update(b"\0");

    // Collect (name, &dep) pairs in sorted order. `Arena::names()`
    // walks the BTreeMap which is already sorted, but we go through a
    // Vec anyway to make the ordering contract local to this
    // function.
    let mut entries: Vec<(&str, &ExternalDepDef)> = deps
        .names()
        .filter_map(|(name, h)| deps.get(h).map(|d| (name, d)))
        .collect();
    entries.sort_by_key(|(name, _)| *name);

    hasher.update((entries.len() as u64).to_le_bytes());
    for (name, dep) in entries {
        // Name
        write_len_prefixed(&mut hasher, name.as_bytes());

        // Source — tagged with a single byte so CratesIo("1.0") and
        // Path("1.0") can never collide.
        match &dep.source {
            DepSource::CratesIo { version } => {
                hasher.update([0u8]);
                write_len_prefixed(&mut hasher, version.as_bytes());
            }
            DepSource::Git { url, reference } => {
                hasher.update([1u8]);
                write_len_prefixed(&mut hasher, url.as_bytes());
                match reference {
                    GitRef::Rev(s) => {
                        hasher.update([0u8]);
                        write_len_prefixed(&mut hasher, s.as_bytes());
                    }
                    GitRef::Tag(s) => {
                        hasher.update([1u8]);
                        write_len_prefixed(&mut hasher, s.as_bytes());
                    }
                    GitRef::Branch(s) => {
                        hasher.update([2u8]);
                        write_len_prefixed(&mut hasher, s.as_bytes());
                    }
                    GitRef::Default => {
                        hasher.update([3u8]);
                    }
                }
            }
            DepSource::Path { path } => {
                hasher.update([2u8]);
                write_len_prefixed(&mut hasher, path.as_bytes());
            }
        }

        // Features — sort locally so insertion-order jitter doesn't
        // perturb the hash. This is belt-and-braces: the Rhai builder
        // appends in script order, which could reasonably vary
        // between formatting refactors.
        let mut sorted_features: Vec<&String> = dep.features.iter().collect();
        sorted_features.sort();
        hasher.update((sorted_features.len() as u64).to_le_bytes());
        for f in sorted_features {
            write_len_prefixed(&mut hasher, f.as_bytes());
        }

        // default_features — single byte is enough.
        hasher.update([dep.default_features as u8]);

        // Target — single-byte tag + len-prefixed name for `Cross`.
        // Resolved to the target's *name*, not its arena index, so the
        // hash is stable across runs (Rhai script reordering can shift
        // indices). A dangling handle hashes as an empty name; the
        // tag byte still distinguishes it from `Host`. The cross
        // pipeline already emits a structured diagnostic for dangling
        // handles upstream, so this branch only matters for the hash's
        // total-function obligation.
        match dep.target {
            TargetSpec::Host => hasher.update([0u8]),
            TargetSpec::Cross(handle) => {
                hasher.update([1u8]);
                let name = targets.get(handle).map(|t| t.name.as_str()).unwrap_or("");
                write_len_prefixed(&mut hasher, name.as_bytes());
            }
        }
    }

    let digest: [u8; 32] = hasher.finalize().into();
    format!("sha256:{}", hex(&digest))
}

fn write_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use quon_model::{DepSource, ExternalDepDef, GitRef, Handle, TargetDef};

    fn dep(name: &str, source: DepSource) -> ExternalDepDef {
        ExternalDepDef {
            name: name.into(),
            source,
            features: Vec::new(),
            default_features: true,
            cfg_flags: Vec::new(),
            rustc_flags: Vec::new(),
            target: quon_model::TargetSpec::Host,
            span: None,
        }
    }

    fn arena_with(entries: Vec<ExternalDepDef>) -> Arena<ExternalDepDef> {
        let mut a = Arena::new();
        for e in entries {
            let name = e.name.clone();
            a.insert(name, e);
        }
        a
    }

    fn no_targets() -> Arena<TargetDef> {
        Arena::new()
    }

    fn targets_with(names: &[&str]) -> Arena<TargetDef> {
        let mut a = Arena::<TargetDef>::new();
        for name in names {
            a.insert(
                (*name).into(),
                TargetDef {
                    name: (*name).into(),
                    spec: "x86_64-unknown-none".into(),
                    builtin: true,
                    panic_strategy: None,
                    span: None,
                },
            );
        }
        a
    }

    #[test]
    fn empty_arena_has_stable_fingerprint() {
        let fp1 = fingerprint_external_deps(&Arena::new(), &no_targets());
        let fp2 = fingerprint_external_deps(&Arena::new(), &no_targets());
        assert_eq!(fp1, fp2);
        assert!(fp1.starts_with("sha256:"));
        assert_eq!(fp1.len(), "sha256:".len() + 64);
    }

    #[test]
    fn insertion_order_does_not_affect_hash() {
        let a = arena_with(vec![
            dep(
                "bitflags",
                DepSource::CratesIo {
                    version: "2.11".into(),
                },
            ),
            dep(
                "log",
                DepSource::CratesIo {
                    version: "0.4".into(),
                },
            ),
        ]);
        let b = arena_with(vec![
            dep(
                "log",
                DepSource::CratesIo {
                    version: "0.4".into(),
                },
            ),
            dep(
                "bitflags",
                DepSource::CratesIo {
                    version: "2.11".into(),
                },
            ),
        ]);
        assert_eq!(
            fingerprint_external_deps(&a, &no_targets()),
            fingerprint_external_deps(&b, &no_targets())
        );
    }

    #[test]
    fn dep_edits_that_change_the_hash() {
        type Edit = fn(&mut ExternalDepDef);
        let cases: [(&str, Edit); 3] = [
            ("version", |d| {
                d.source = DepSource::CratesIo {
                    version: "2".into(),
                }
            }),
            ("feature added", |d| d.features.push("std".into())),
            ("default features off", |d| d.default_features = false),
        ];
        for (name, edit) in cases {
            let a = dep(
                "x",
                DepSource::CratesIo {
                    version: "1".into(),
                },
            );
            let mut b = a.clone();
            edit(&mut b);
            assert_ne!(
                fingerprint_external_deps(&arena_with(vec![a]), &no_targets()),
                fingerprint_external_deps(&arena_with(vec![b]), &no_targets()),
                "{name} must change the hash"
            );
        }
    }

    #[test]
    fn dep_edits_that_keep_the_hash() {
        // cfg and rustc flags change how a dep compiles but not what
        // vendoring writes, so editing one must not force a re-vendor.
        type Edit = fn(&mut ExternalDepDef);
        let cases: [(&str, Edit, Edit); 3] = [
            (
                "feature order",
                |d| d.features = vec!["std".into(), "serde".into()],
                |d| d.features = vec!["serde".into(), "std".into()],
            ),
            (
                "cfg and rustc flags",
                |_| {},
                |d| {
                    d.cfg_flags = vec!["feature=\"foo\"".into()];
                    d.rustc_flags = vec!["-C".into(), "opt-level=3".into()];
                },
            ),
            (
                "span",
                |_| {},
                |d| d.span = Some(quon_model::SourceSpan::point("fake.rs", 1, 1)),
            ),
        ];
        for (name, edit_a, edit_b) in cases {
            let base = dep(
                "x",
                DepSource::CratesIo {
                    version: "1".into(),
                },
            );
            let (mut a, mut b) = (base.clone(), base);
            edit_a(&mut a);
            edit_b(&mut b);
            assert_eq!(
                fingerprint_external_deps(&arena_with(vec![a]), &no_targets()),
                fingerprint_external_deps(&arena_with(vec![b]), &no_targets()),
                "{name} must not change the hash"
            );
        }
    }

    #[test]
    fn source_variant_tagging_prevents_collisions() {
        // A CratesIo("foo") dep must not hash the same as a Path("foo")
        // dep — this is what the single-byte source tag is for.
        let a = arena_with(vec![dep(
            "x",
            DepSource::CratesIo {
                version: "foo".into(),
            },
        )]);
        let b = arena_with(vec![dep("x", DepSource::Path { path: "foo".into() })]);
        assert_ne!(
            fingerprint_external_deps(&a, &no_targets()),
            fingerprint_external_deps(&b, &no_targets())
        );
    }

    #[test]
    fn git_ref_variants_are_distinguished() {
        let url = "https://example.com/x.git".to_string();
        let tag = arena_with(vec![dep(
            "x",
            DepSource::Git {
                url: url.clone(),
                reference: GitRef::Tag("v1".into()),
            },
        )]);
        let branch = arena_with(vec![dep(
            "x",
            DepSource::Git {
                url: url.clone(),
                reference: GitRef::Branch("v1".into()),
            },
        )]);
        let rev = arena_with(vec![dep(
            "x",
            DepSource::Git {
                url: url.clone(),
                reference: GitRef::Rev("v1".into()),
            },
        )]);
        let default = arena_with(vec![dep(
            "x",
            DepSource::Git {
                url,
                reference: GitRef::Default,
            },
        )]);
        let ftag = fingerprint_external_deps(&tag, &no_targets());
        let fbranch = fingerprint_external_deps(&branch, &no_targets());
        let frev = fingerprint_external_deps(&rev, &no_targets());
        let fdefault = fingerprint_external_deps(&default, &no_targets());
        assert_ne!(ftag, fbranch);
        assert_ne!(ftag, frev);
        assert_ne!(ftag, fdefault);
        assert_ne!(fbranch, frev);
    }

    #[test]
    fn host_to_cross_perturbs_hash() {
        // The headline acceptance criterion for slice 8: adding
        // `.target("kernel")` to an existing dep must invalidate the
        // fingerprint and force a re-vendor.
        let host_arena = arena_with(vec![dep(
            "x",
            DepSource::CratesIo {
                version: "1".into(),
            },
        )]);

        let targets = targets_with(&["kernel"]);
        let kernel = targets.lookup("kernel").expect("inserted");
        let mut cross_dep = dep(
            "x",
            DepSource::CratesIo {
                version: "1".into(),
            },
        );
        cross_dep.target = TargetSpec::Cross(kernel);
        let cross_arena = arena_with(vec![cross_dep]);

        assert_ne!(
            fingerprint_external_deps(&host_arena, &no_targets()),
            fingerprint_external_deps(&cross_arena, &targets),
        );
    }

    #[test]
    fn cross_target_name_perturbs_hash() {
        // Same dep, retargeted from `kernel` to `graphics`. Both arms
        // are `Cross(_)`, but the resolved target *name* differs, so
        // the fingerprint must differ.
        let targets = targets_with(&["kernel", "graphics"]);
        let kernel = targets.lookup("kernel").expect("inserted");
        let graphics = targets.lookup("graphics").expect("inserted");

        let mut a = dep(
            "x",
            DepSource::CratesIo {
                version: "1".into(),
            },
        );
        a.target = TargetSpec::Cross(kernel);

        let mut b = dep(
            "x",
            DepSource::CratesIo {
                version: "1".into(),
            },
        );
        b.target = TargetSpec::Cross(graphics);

        assert_ne!(
            fingerprint_external_deps(&arena_with(vec![a]), &targets),
            fingerprint_external_deps(&arena_with(vec![b]), &targets),
        );
    }

    #[test]
    fn cross_handle_index_does_not_affect_hash() {
        // Two `targets` arenas with the same target name but different
        // arena insertion orders (so the kernel handle has different
        // indices). The fingerprint must be identical because we hash
        // the resolved *name*, not the handle index.
        let mut targets_a = Arena::<TargetDef>::new();
        targets_a.insert(
            "kernel".into(),
            TargetDef {
                name: "kernel".into(),
                spec: "x86_64-unknown-none".into(),
                builtin: true,
                panic_strategy: None,
                span: None,
            },
        );
        let kernel_a = targets_a.lookup("kernel").expect("inserted");

        let mut targets_b = Arena::<TargetDef>::new();
        // Insert a decoy first so `kernel` lands at a different index.
        targets_b.insert(
            "decoy".into(),
            TargetDef {
                name: "decoy".into(),
                spec: "aarch64-unknown-none".into(),
                builtin: true,
                panic_strategy: None,
                span: None,
            },
        );
        targets_b.insert(
            "kernel".into(),
            TargetDef {
                name: "kernel".into(),
                spec: "x86_64-unknown-none".into(),
                builtin: true,
                panic_strategy: None,
                span: None,
            },
        );
        let kernel_b = targets_b.lookup("kernel").expect("inserted");
        assert_ne!(
            kernel_a.index(),
            kernel_b.index(),
            "test setup: handles must differ"
        );

        let mut da = dep(
            "x",
            DepSource::CratesIo {
                version: "1".into(),
            },
        );
        da.target = TargetSpec::Cross(kernel_a);
        let mut db = dep(
            "x",
            DepSource::CratesIo {
                version: "1".into(),
            },
        );
        db.target = TargetSpec::Cross(kernel_b);

        assert_eq!(
            fingerprint_external_deps(&arena_with(vec![da]), &targets_a),
            fingerprint_external_deps(&arena_with(vec![db]), &targets_b),
        );
    }

    #[test]
    fn cross_target_spec_change_does_not_perturb_hash() {
        // Changing `target_spec("kernel", "...")` from one triple to
        // another while keeping the name `"kernel"` does NOT change
        // the fingerprint: vendor output is keyed by target name in
        // our setup, and per-node compile stamps catch spec changes
        // separately. This is documented behaviour — see the module
        // docstring.
        let mut targets_a = Arena::<TargetDef>::new();
        targets_a.insert(
            "kernel".into(),
            TargetDef {
                name: "kernel".into(),
                spec: "x86_64-unknown-none".into(),
                builtin: true,
                panic_strategy: None,
                span: None,
            },
        );
        let kernel_a = targets_a.lookup("kernel").expect("inserted");

        let mut targets_b = Arena::<TargetDef>::new();
        targets_b.insert(
            "kernel".into(),
            TargetDef {
                name: "kernel".into(),
                spec: "aarch64-unknown-none".into(),
                builtin: false,
                panic_strategy: None,
                span: None,
            },
        );
        let kernel_b = targets_b.lookup("kernel").expect("inserted");

        let mut da = dep(
            "x",
            DepSource::CratesIo {
                version: "1".into(),
            },
        );
        da.target = TargetSpec::Cross(kernel_a);
        let mut db = dep(
            "x",
            DepSource::CratesIo {
                version: "1".into(),
            },
        );
        db.target = TargetSpec::Cross(kernel_b);

        assert_eq!(
            fingerprint_external_deps(&arena_with(vec![da]), &targets_a),
            fingerprint_external_deps(&arena_with(vec![db]), &targets_b),
        );
    }

    #[test]
    fn dangling_cross_handle_is_total_function() {
        // A dangling target handle (handle index outside `targets`)
        // shouldn't panic the hash. The cross context-builder rejects
        // these upstream with a structured diagnostic, but the
        // fingerprint must remain a total function so any path that
        // hashes a half-validated model still returns a value.
        let mut d = dep(
            "x",
            DepSource::CratesIo {
                version: "1".into(),
            },
        );
        d.target = TargetSpec::Cross(Handle::<TargetDef>::new(99));
        let fp = fingerprint_external_deps(&arena_with(vec![d]), &no_targets());
        assert!(fp.starts_with("sha256:"));
    }
}
