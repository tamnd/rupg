//! The change of the owner of an object: `ATExecChangeOwner` for one relation and `AlterSchemaOwner_internal` for a schema, with `aclnewowner` for the privileges. The analyzer runs the checks and the walk to the indexes, the TOAST table and the sequences of a table.

use std::sync::Arc;

use rupg_common::Result;

use super::{Catalog, no_object};
use crate::{
    ObjRef, PG_COLLATION, PG_CONVERSION, PG_EXTENSION, PG_LANGUAGE, PG_OPCLASS, PG_OPERATOR,
    PG_OPFAMILY, PG_PROC, PG_STATISTIC_EXT, PG_TS_CONFIG, PG_TS_DICT, PG_TYPE,
};

/// `ACL_ALL_RIGHTS_STR`: the letter of each privilege, in the order of its bit.
const RIGHTS: &[u8] = b"arwdDxtXUCTcsAm";

/// One `aclitem` in its parts: the grantee, the grantor, and the bits of the privileges and of their grant options. An empty grantee is `PUBLIC`.
struct Item {
    grantee: String,
    grantor: String,
    rights: u32,
    options: u32,
}

impl Item {
    /// The parts of the text form `grantee=privileges/grantor`. The names of the built-in roles need no quotes, so the parts are the text between `=` and `/`.
    fn parse(text: &str) -> Option<Item> {
        let (grantee, rest) = text.split_once('=')?;
        let (privileges, grantor) = rest.rsplit_once('/')?;
        let (mut rights, mut options) = (0, 0);
        let mut last = None;
        for b in privileges.bytes() {
            if b == b'*' {
                options |= last?;
                continue;
            }
            let bit = 1 << RIGHTS.iter().position(|&r| r == b)?;
            rights |= bit;
            last = Some(bit);
        }
        Some(Item { grantee: grantee.into(), grantor: grantor.into(), rights, options })
    }

    /// The text form, as `aclitemout` writes it.
    fn text(&self) -> String {
        let mut out = format!("{}=", self.grantee);
        for (i, &letter) in RIGHTS.iter().enumerate() {
            if self.rights & (1 << i) != 0 {
                out.push(char::from(letter));
                if self.options & (1 << i) != 0 {
                    out.push('*');
                }
            }
        }
        out.push('/');
        out.push_str(&self.grantor);
        out
    }
}

/// `aclnewowner`: the items with the old owner as grantee or grantor get the new owner. When the new owner was in the list before, two items with the same grantee and grantor become one, and an item with no privileges goes away. An item that is not in the text form stays as it is.
pub fn acl_new_owner(acl: &[String], old: &str, new: &str) -> Vec<String> {
    let mut items: Vec<(Option<Item>, &String)> = Vec::with_capacity(acl.len());
    let mut new_present = false;
    for text in acl {
        let mut item = Item::parse(text);
        if let Some(item) = &mut item {
            for name in [&mut item.grantee, &mut item.grantor] {
                if name == new {
                    new_present = true;
                } else if name == old {
                    *name = new.to_string();
                }
            }
        }
        items.push((item, text));
    }
    if new_present {
        for dst in 0..items.len() {
            let Some(item) = &items[dst].0 else { continue };
            if item.rights == 0 {
                continue;
            }
            let (grantee, grantor) = (item.grantee.clone(), item.grantor.clone());
            let (mut rights, mut options) = (0, 0);
            for (src, _) in items.iter_mut().skip(dst + 1) {
                if let Some(src) = src
                    && src.grantee == grantee
                    && src.grantor == grantor
                {
                    rights |= src.rights;
                    options |= src.options;
                    src.rights = 0;
                    src.options = 0;
                }
            }
            if let Some(item) = &mut items[dst].0 {
                item.rights |= rights;
                item.options |= options;
            }
        }
        items.retain(|(item, _)| !matches!(item, Some(item) if item.rights == 0));
    }
    items
        .into_iter()
        .map(|(item, text)| item.map_or_else(|| text.clone(), |item| item.text()))
        .collect()
}

impl Catalog {
    /// The change of `ATExecChangeOwner` for one relation: `relowner`, `relacl`, and the owner of the row type and of its array type, as `AlterTypeOwnerInternal` changes them. `old` and `new` are the names of the two roles, for the items of the ACL.
    pub fn set_relation_owner(&mut self, oid: u32, owner: u32, old: &str, new: &str) -> Result<()> {
        let rel = self.relation_mut(oid)?;
        rel.owner = owner;
        if let Some(acl) = &rel.acl {
            rel.acl = Some(acl_new_owner(acl, old, new));
        }
        let row_type = rel.row_type;
        let array = match self.types.get_mut(&row_type) {
            Some(ty) => {
                ty.owner = owner;
                ty.array
            }
            None => 0,
        };
        if let Some(ty) = self.types.get_mut(&array) {
            ty.owner = owner;
        }
        Ok(())
    }

    /// The change of `AlterSchemaOwner_internal`: `nspowner` and `nspacl`. `old` and `new` are the names of the two roles, for the items of the ACL.
    pub fn set_schema_owner(&mut self, oid: u32, owner: u32, old: &str, new: &str) -> Result<()> {
        let schema = self.schemas.get_mut(&oid).ok_or_else(|| no_object("schema", oid))?;
        schema.owner = owner;
        if let Some(acl) = &schema.acl {
            schema.acl = Some(acl_new_owner(acl, old, new));
        }
        Ok(())
    }

    /// The change of `AlterObjectOwner_internal` for a function, a collation, a language, an extension, an operator, an operator class, an operator family, a statistics object, a text search dictionary and a text search configuration, and of `AlterTypeOwnerInternal` for the ACL of a type. The owner column changes, and the ACL that `GRANT` gave gets `aclnewowner`. The caller changes the owner of a type with [`Catalog::set_type_owner`] first. `old` and `new` are the names of the two roles, for the items of the ACL.
    ///
    /// # Errors
    ///
    /// An internal error for an object that the catalog does not have.
    pub fn set_object_owner(
        &mut self,
        object: ObjRef,
        owner: u32,
        old: &str,
        new: &str,
    ) -> Result<()> {
        let found = match object.class {
            PG_TYPE => true,
            PG_PROC => self.functions.get_mut(&object.oid).map(|f| f.owner = owner).is_some(),
            PG_COLLATION => match Arc::make_mut(&mut self.collations).get_mut(&object.oid) {
                Some(c) => {
                    c.owner = owner;
                    true
                }
                None => false,
            },
            PG_LANGUAGE => self.languages.get_mut(&object.oid).map(|l| l.owner = owner).is_some(),
            PG_EXTENSION => self.extensions.get_mut(&object.oid).map(|e| e.owner = owner).is_some(),
            PG_OPERATOR => self.operators.get_mut(&object.oid).map(|o| o.owner = owner).is_some(),
            PG_OPCLASS => self.opclasses.get_mut(&object.oid).map(|c| c.owner = owner).is_some(),
            PG_OPFAMILY => self.opfamilies.get_mut(&object.oid).map(|f| f.owner = owner).is_some(),
            PG_STATISTIC_EXT => {
                self.stat_exts.get_mut(&object.oid).map(|s| s.owner = owner).is_some()
            }
            PG_TS_DICT => {
                self.ts_dictionaries.get_mut(&object.oid).map(|d| d.owner = owner).is_some()
            }
            PG_TS_CONFIG => self.ts_configs.get_mut(&object.oid).map(|c| c.owner = owner).is_some(),
            PG_CONVERSION => {
                self.conversions.get_mut(&object.oid).map(|c| c.owner = owner).is_some()
            }
            _ => false,
        };
        if !found {
            return Err(no_object("object", object.oid));
        }
        if let Some(acl) = self.object_acl(object) {
            let acl = acl_new_owner(acl, old, new);
            self.set_object_acl(object, Some(acl));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acl(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_new_owner_takes_the_items_of_the_old_owner() {
        let old = acl(&["alice=arwdDxtm/alice", "=r/alice", "bob=r*/alice"]);
        assert_eq!(
            acl_new_owner(&old, "alice", "carol"),
            ["carol=arwdDxtm/carol", "=r/carol", "bob=r*/carol"]
        );
    }

    #[test]
    fn the_items_of_the_new_owner_merge() {
        let old = acl(&["alice=arwdDxtm/alice", "bob=r/alice", "bob=w*/bob"]);
        assert_eq!(acl_new_owner(&old, "alice", "bob"), ["bob=arw*dDxtm/bob"]);
        let old = acl(&["bob=r/alice", "bob=a*/bob", "alice=UC/alice"]);
        assert_eq!(acl_new_owner(&old, "alice", "bob"), ["bob=a*rUC/bob"]);
        let old = acl(&["=U/alice", "bob=X/carol"]);
        assert_eq!(acl_new_owner(&old, "alice", "bob"), ["=U/bob", "bob=X/carol"]);
    }
}
