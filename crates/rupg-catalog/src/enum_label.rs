//! `ALTER TYPE ... ADD VALUE` and `ALTER TYPE ... RENAME VALUE` of an enum type: the changes of `AddEnumLabel`, `RenumberEnumType` and `RenameEnumLabel`, and the labels that a statement cannot use before the commit.

use rupg_common::Result;

use super::{Catalog, EnumLabel, no_object};

impl Catalog {
    /// `AddEnumLabel`: adds the label at the end, or before or after the label at the place `neighbor` of the sort order. The new label gets the middle of the sort orders of its neighbors. When the precision of `float4` gives no middle, `RenumberEnumType` gives the labels the sort orders 1, 2, 3 and so on, and the search starts again. The new label takes the next OID, but an even OID that does not sort correctly against the even OIDs of the other labels is skipped, so that the label gets the odd OID after it. The caller checks the name.
    ///
    /// # Errors
    ///
    /// An internal error for a type that is not an enum type of the catalog.
    #[allow(clippy::cast_precision_loss, clippy::manual_midpoint)]
    pub fn add_enum_label(
        &mut self,
        ty: u32,
        label: String,
        neighbor: Option<(usize, bool)>,
    ) -> Result<()> {
        let labels = self.labels_mut(ty)?;
        let (order, at) = loop {
            let Some((at, after)) = neighbor else {
                break (labels.last().map_or(1.0, |l| l.sort_order + 1.0), labels.len());
            };
            let near = labels[at].sort_order;
            let other = if after { at.checked_add(1) } else { at.checked_sub(1) };
            let place = if after { at + 1 } else { at };
            let Some(other) = other.and_then(|o| labels.get(o)).map(|l| l.sort_order) else {
                break (if after { near + 1.0 } else { near - 1.0 }, place);
            };
            // The sum rounds to `float4` before the division, as in C.
            let middle = (near + other) / 2.0;
            if middle != near && middle != other {
                break (middle, place);
            }
            for (i, label) in labels.iter_mut().enumerate() {
                label.sort_order = (i + 1) as f32;
            }
        };
        let even: Vec<(u32, f32)> =
            labels.iter().filter(|l| l.oid & 1 == 0).map(|l| (l.oid, l.sort_order)).collect();
        let oid = loop {
            let oid = self.new_oid();
            let sorts = even.iter().all(|&(e, o)| if o < order { e < oid } else { e > oid });
            if sorts || oid & 1 == 1 {
                break oid;
            }
        };
        self.labels_mut(ty)?.insert(at, EnumLabel { oid, sort_order: order, label });
        if !self.uncommitted_types.contains(&ty) {
            self.uncommitted_labels.push(oid);
        }
        Ok(())
    }

    /// `RenameEnumLabel`: the label at the place `at` of the sort order gets the new name. The caller checks the names.
    ///
    /// # Errors
    ///
    /// An internal error for a type that is not an enum type of the catalog.
    pub fn rename_enum_label(&mut self, ty: u32, at: usize, name: String) -> Result<()> {
        if let Some(label) = self.labels_mut(ty)?.get_mut(at) {
            label.label = name;
        }
        Ok(())
    }

    /// `EnumUncommitted`: true for a label that `ALTER TYPE ... ADD VALUE` of the open transaction gave to an enum type that an earlier transaction made.
    pub fn label_is_uncommitted(&self, label: u32) -> bool {
        self.uncommitted_labels.contains(&label)
    }

    /// True when the open transaction made an enum type or gave a label to one, so that the commit has a change for [`Catalog::commit_enums`].
    pub fn has_uncommitted_enums(&self) -> bool {
        !self.uncommitted_types.is_empty() || !self.uncommitted_labels.is_empty()
    }

    /// `AtEOXact_Enum`: after the commit, each enum type and each label is one of an earlier transaction.
    pub fn commit_enums(&mut self) {
        self.uncommitted_types.clear();
        self.uncommitted_labels.clear();
    }

    /// The labels of an enum type of the catalog.
    fn labels_mut(&mut self, ty: u32) -> Result<&mut Vec<EnumLabel>> {
        self.types.get_mut(&ty).and_then(|t| t.labels.as_mut()).ok_or_else(|| no_object("type", ty))
    }
}
