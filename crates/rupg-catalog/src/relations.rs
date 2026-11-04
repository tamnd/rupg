//! The relations of the catalog by OID, with a map by name, a map of the indexes of each table and a map of the children of each table that a read builds once.
//!
//! The statements find a relation by name and the indexes of a table many times, and a scan of all the relations costs more as the database grows. A change of the relations drops the maps: each change goes through [`DerefMut`], so no change can keep a stale map.

use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasherDefault, Hasher};
use std::ops::{Deref, DerefMut};
use std::sync::OnceLock;

use rupg_common::Result;
use rupg_common::persist::{Persist, Reader};

use crate::Relation;

/// A hasher for OIDs and names, which costs less than SipHash. The keys come from the catalog, so the maps need no guard against keys that an attacker selects.
#[derive(Default)]
struct OidHasher(u64);

impl Hasher for OidHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(u64::from(byte));
        }
    }

    fn write_u32(&mut self, word: u32) {
        self.write_u64(u64::from(word));
    }

    fn write_u64(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

/// A map with [`OidHasher`].
type OidMap<K, V> = HashMap<K, V, BuildHasherDefault<OidHasher>>;

/// The maps that a read builds from the relations.
#[derive(Debug, Default)]
struct Maps {
    /// The OID of each relation by schema and name. A name that two relations have gives the lower OID, as a scan in the order of the OIDs finds it.
    names: OidMap<u32, OidMap<Box<str>, u32>>,
    /// The OIDs of the indexes of each table, in their order.
    indexes: OidMap<u32, Vec<u32>>,
    /// The OIDs of the partitions of each table and of the tables that inherit from it, in their order.
    children: OidMap<u32, Vec<u32>>,
}

/// The relations by OID.
#[derive(Default)]
pub(crate) struct Relations {
    map: BTreeMap<u32, Relation>,
    maps: OnceLock<Maps>,
}

impl Relations {
    fn maps(&self) -> &Maps {
        self.maps.get_or_init(|| {
            let mut maps = Maps::default();
            for (&oid, rel) in &self.map {
                maps.names
                    .entry(rel.namespace)
                    .or_default()
                    .entry(rel.name.as_str().into())
                    .or_insert(oid);
                if let Some(info) = &rel.index {
                    maps.indexes.entry(info.table).or_default().push(oid);
                }
                let parents =
                    rel.partition_of.iter().map(|p| p.parent).chain(rel.inherits.iter().copied());
                for parent in parents {
                    let children = maps.children.entry(parent).or_default();
                    if children.last() != Some(&oid) {
                        children.push(oid);
                    }
                }
            }
            maps
        })
    }

    /// The relation with the name in the schema.
    pub(crate) fn by_name(&self, namespace: u32, name: &str) -> Option<&Relation> {
        let oid = self.maps().names.get(&namespace)?.get(name)?;
        self.map.get(oid)
    }

    /// The indexes of the table `table`, in the order of their OIDs.
    pub(crate) fn indexes_of(&self, table: u32) -> impl Iterator<Item = &Relation> {
        let oids = self.maps().indexes.get(&table).map_or(&[][..], Vec::as_slice);
        oids.iter().filter_map(|oid| self.map.get(oid))
    }

    /// The partitions of the table `parent` and the tables that inherit from it, in the order of their OIDs.
    pub(crate) fn children_of(&self, parent: u32) -> impl Iterator<Item = &Relation> {
        let oids = self.maps().children.get(&parent).map_or(&[][..], Vec::as_slice);
        oids.iter().filter_map(|oid| self.map.get(oid))
    }
}

impl Deref for Relations {
    type Target = BTreeMap<u32, Relation>;

    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl DerefMut for Relations {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.maps.take();
        &mut self.map
    }
}

impl<'a> IntoIterator for &'a Relations {
    type Item = (&'a u32, &'a Relation);
    type IntoIter = std::collections::btree_map::Iter<'a, u32, Relation>;

    fn into_iter(self) -> Self::IntoIter {
        self.map.iter()
    }
}

impl Clone for Relations {
    fn clone(&self) -> Self {
        Relations { map: self.map.clone(), maps: OnceLock::new() }
    }
}

impl PartialEq for Relations {
    fn eq(&self, other: &Self) -> bool {
        self.map == other.map
    }
}

impl Eq for Relations {}

impl std::fmt::Debug for Relations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.map.fmt(f)
    }
}

impl Persist for Relations {
    fn put(&self, out: &mut Vec<u8>) {
        self.map.put(out);
    }

    fn take(input: &mut Reader<'_>) -> Result<Self> {
        Ok(Relations { map: Persist::take(input)?, maps: OnceLock::new() })
    }
}
