//! The aggregate functions of `pg_aggregate`, as `AggregateCreate` of `pg_aggregate.c` makes them. The function of an aggregate is a row of `pg_proc` with `prokind` `a`.

use rupg_common::Result;

use super::{Catalog, Function, ObjRef};

/// The OID of `pg_aggregate`.
pub const PG_AGGREGATE: u32 = 2600;

/// An aggregate function, one row of `pg_aggregate`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Aggregate {
    /// `aggfnoid`: the OID of the function in `pg_proc`. [`Catalog::create_aggregate`] sets it.
    pub function: u32,
    /// `aggkind`: `n` for a normal aggregate, `o` for an ordered-set aggregate or `h` for a hypothetical-set aggregate.
    pub kind: u8,
    /// `aggnumdirectargs`: the number of direct arguments of an ordered-set aggregate.
    pub ndirect: i16,
    /// `aggtransfn`: the transition function.
    pub transfn: u32,
    /// `aggfinalfn`: the final function, or 0.
    pub finalfn: u32,
    /// `aggcombinefn`: the combine function, or 0.
    pub combinefn: u32,
    /// `aggserialfn`: the serialization function, or 0.
    pub serialfn: u32,
    /// `aggdeserialfn`: the deserialization function, or 0.
    pub deserialfn: u32,
    /// `aggmtransfn`: the forward transition function of the moving-aggregate mode, or 0.
    pub mtransfn: u32,
    /// `aggminvtransfn`: the inverse transition function of the moving-aggregate mode, or 0.
    pub minvtransfn: u32,
    /// `aggmfinalfn`: the final function of the moving-aggregate mode, or 0.
    pub mfinalfn: u32,
    /// `aggfinalextra`: the final function gets a null for each argument of the aggregate after the state.
    pub finalextra: bool,
    /// `aggmfinalextra`: the same for the final function of the moving-aggregate mode.
    pub mfinalextra: bool,
    /// `aggfinalmodify`: `r` read-only, `s` shareable or `w` read-write.
    pub finalmodify: u8,
    /// `aggmfinalmodify`: the same for the final function of the moving-aggregate mode.
    pub mfinalmodify: u8,
    /// `aggsortop`: the sort operator, or 0.
    pub sortop: u32,
    /// `aggtranstype`: the type of the state.
    pub transtype: u32,
    /// `aggtransspace`: the estimate of the size of the state, or 0.
    pub transspace: i32,
    /// `aggmtranstype`: the type of the state of the moving-aggregate mode, or 0.
    pub mtranstype: u32,
    /// `aggmtransspace`: the estimate of the size of that state, or 0.
    pub mtransspace: i32,
    /// `agginitval`: the text of the first state, or `None` for null.
    pub initval: Option<String>,
    /// `aggminitval`: the text of the first state of the moving-aggregate mode, or `None` for null.
    pub minitval: Option<String>,
}

impl Catalog {
    /// `AggregateCreate`: the row of `pg_proc` of the aggregate, which gets the next OID, and its row of `pg_aggregate`. The function depends on its schema and on the objects in `refs`, which are its types, its support functions and its sort operator.
    ///
    /// # Errors
    ///
    /// `42723` when the schema has a function with the name and the argument types.
    pub fn create_aggregate(
        &mut self,
        function: Function,
        mut aggregate: Aggregate,
        refs: &[ObjRef],
    ) -> Result<u32> {
        let oid = self.create_function(function, refs)?;
        aggregate.function = oid;
        self.aggregates.insert(oid, aggregate);
        Ok(oid)
    }

    /// `AggregateCreate` with `replace` for the aggregate `oid` that exists: its function and its row of `pg_aggregate` get the new facts, and its dependencies are new.
    ///
    /// # Errors
    ///
    /// An internal error when no function has the OID.
    pub fn replace_aggregate(
        &mut self,
        oid: u32,
        function: Function,
        mut aggregate: Aggregate,
        refs: &[ObjRef],
    ) -> Result<()> {
        self.replace_function(oid, function, refs)?;
        aggregate.function = oid;
        self.aggregates.insert(oid, aggregate);
        Ok(())
    }

    /// The aggregates, in the order of the OIDs of their functions.
    pub fn aggregates(&self) -> impl Iterator<Item = &Aggregate> {
        self.aggregates.values()
    }

    /// The row of `pg_aggregate` of the function with the OID.
    pub fn aggregate(&self, function: u32) -> Option<&Aggregate> {
        self.aggregates.get(&function)
    }
}

rupg_common::persist_struct!(Aggregate {
    function,
    kind,
    ndirect,
    transfn,
    finalfn,
    combinefn,
    serialfn,
    deserialfn,
    mtransfn,
    minvtransfn,
    mfinalfn,
    finalextra,
    mfinalextra,
    finalmodify,
    mfinalmodify,
    sortop,
    transtype,
    transspace,
    mtranstype,
    mtransspace,
    initval,
    minitval
});
