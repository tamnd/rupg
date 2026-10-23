//! The operators of `pg_operator` that `CREATE OPERATOR` makes, as `OperatorCreate`, `OperatorShellMake`, `OperatorUpd` and `makeOperatorDependencies` of `pg_operator.c` make them.

use rupg_common::Result;

use super::{Catalog, ObjRef, PG_NAMESPACE, PG_PROC, PG_TYPE, no_object};

/// The OID of `pg_operator`.
pub const PG_OPERATOR: u32 = 2617;

/// An operator, one row of `pg_operator`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operator {
    /// The OID. [`Catalog::create_operator`] sets it.
    pub oid: u32,
    /// The name.
    pub name: String,
    /// The OID of the schema.
    pub namespace: u32,
    /// The OID of the owner role.
    pub owner: u32,
    /// `oprkind`: `b` for an infix operator or `l` for a prefix operator.
    pub kind: u8,
    /// `oprcanmerge`.
    pub can_merge: bool,
    /// `oprcanhash`.
    pub can_hash: bool,
    /// `oprleft`: the type of the left input, or 0 for a prefix operator.
    pub left: u32,
    /// `oprright`.
    pub right: u32,
    /// `oprresult`, or 0 for a shell.
    pub result: u32,
    /// `oprcom`: the commutator, or 0.
    pub commutator: u32,
    /// `oprnegate`: the negator, or 0.
    pub negator: u32,
    /// `oprcode`: the function, or 0 for a shell.
    pub code: u32,
    /// `oprrest`: the restriction estimator, or 0.
    pub restrict: u32,
    /// `oprjoin`: the join estimator, or 0.
    pub join: u32,
}

impl Operator {
    /// `OperatorShellMake`: an operator with a name and input types only, which a later `CREATE OPERATOR` fills.
    pub fn shell(name: &str, namespace: u32, owner: u32, left: u32, right: u32) -> Operator {
        Operator {
            oid: 0,
            name: name.to_string(),
            namespace,
            owner,
            kind: if left == 0 { b'l' } else { b'b' },
            can_merge: false,
            can_hash: false,
            left,
            right,
            result: 0,
            commutator: 0,
            negator: 0,
            code: 0,
            restrict: 0,
            join: 0,
        }
    }
}

impl Catalog {
    /// The operators of the catalog, in the order of their OIDs.
    pub fn operators(&self) -> impl Iterator<Item = &Operator> {
        self.operators.values()
    }

    /// The operator with the OID.
    pub fn operator(&self, oid: u32) -> Option<&Operator> {
        self.operators.get(&oid)
    }

    /// `OperatorGet`: the operator of the schema with the name and the input types.
    pub fn operator_by_name(
        &self,
        namespace: u32,
        name: &str,
        left: u32,
        right: u32,
    ) -> Option<&Operator> {
        self.operators.values().find(|o| {
            o.namespace == namespace && o.name == name && o.left == left && o.right == right
        })
    }

    /// `OperatorCreate` and `OperatorShellMake` for a new operator, which gets the next OID, with the dependencies of `makeOperatorDependencies`.
    pub fn create_operator(&mut self, mut new: Operator) -> u32 {
        let oid = self.new_oid();
        new.oid = oid;
        self.operators.insert(oid, new);
        self.operator_depends(oid);
        oid
    }

    /// `OperatorCreate` for a shell operator that exists: the operator gets the new facts and keeps its OID, and its dependencies are made again.
    ///
    /// # Errors
    ///
    /// An internal error when no operator has the OID.
    pub fn replace_operator(&mut self, oid: u32, mut new: Operator) -> Result<()> {
        if !self.operators.contains_key(&oid) {
            return Err(no_object("operator", oid));
        }
        new.oid = oid;
        self.operators.insert(oid, new);
        let me = ObjRef::new(PG_OPERATOR, oid);
        self.depends.retain(|d| d.object != me);
        self.operator_depends(oid);
        Ok(())
    }

    /// Changes the row of the operator, as `OperatorUpd` and `AlterOperator` change it.
    ///
    /// # Errors
    ///
    /// An internal error when no operator has the OID.
    pub fn alter_operator(&mut self, oid: u32, change: impl FnOnce(&mut Operator)) -> Result<()> {
        let operator = self.operators.get_mut(&oid).ok_or_else(|| no_object("operator", oid))?;
        change(operator);
        Ok(())
    }

    /// `RemoveOperatorById`: the commutator and the negator of the operator lose their link to it, as `OperatorUpd` with `isDelete` clears them, and the operator goes.
    pub(crate) fn remove_operator(&mut self, oid: u32) {
        let Some(old) = self.operators.remove(&oid) else { return };
        if let Some(commutator) = self.operators.get_mut(&old.commutator) {
            commutator.commutator = 0;
        }
        if let Some(negator) = self.operators.get_mut(&old.negator) {
            negator.negator = 0;
        }
    }

    /// `makeOperatorDependencies`: the operator depends on its schema, its types and its functions.
    fn operator_depends(&mut self, oid: u32) {
        let Some(op) = self.operators.get(&oid) else { return };
        let mut refs = vec![ObjRef::new(PG_NAMESPACE, op.namespace)];
        refs.extend(
            [op.left, op.right, op.result]
                .into_iter()
                .filter(|&ty| ty != 0)
                .map(|ty| ObjRef::new(PG_TYPE, ty)),
        );
        refs.extend(
            [op.code, op.restrict, op.join]
                .into_iter()
                .filter(|&f| f != 0)
                .map(|f| ObjRef::new(PG_PROC, f)),
        );
        self.depend_all(ObjRef::new(PG_OPERATOR, oid), refs);
    }
}
