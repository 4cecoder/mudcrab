// Differential SQL and failure regressions for the canonical binding diagnostic.
#[cfg(test)]
mod sqlite_controls {
    use super::{BindSite, CanonicalBindProfile};
    use rusqlite::{Connection, Error, Result, Statement, ToSql, params};
    use rusqlite::types::{ToSqlOutput, Value};

    const INSERT: &str = "INSERT OR REPLACE INTO records(form_id, record_type, cell_id, worldspace_id, load_order, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
    struct Row { id: u32, tag: &'static str, cell: Option<u32>, world: Option<u32>, priority: u32, data: Vec<u8> }

    fn database() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE records(form_id INTEGER PRIMARY KEY,record_type TEXT NOT NULL,cell_id INTEGER,worldspace_id INTEGER,load_order INTEGER NOT NULL,data BLOB NOT NULL CHECK(length(data)<32));CREATE INDEX idx_records_type ON records(record_type);CREATE INDEX idx_records_cell_id ON records(cell_id) WHERE cell_id IS NOT NULL;").unwrap();
        conn
    }

    fn old(statement: &mut Statement<'_>, row: &Row) -> Result<usize> {
        let changed = statement.execute(params![row.id,row.tag,row.cell,row.world,row.priority,row.data])?;
        statement.clear_bindings(); Ok(changed)
    }

    fn checked(statement: &Statement<'_>) -> Result<()> {
        let expected = statement.parameter_count();
        if expected == 6 { Ok(()) } else { Err(Error::InvalidParameterCount(6.min(expected.saturating_add(1)),expected)) }
    }

    fn new(statement: &mut Statement<'_>, row: &Row, profile: &mut CanonicalBindProfile) -> Result<usize> {
        checked(statement)?;
        profile.attempt_row(row.data.len());
        let start = profile.begin();
        let bound = (|| -> Result<()> {
            statement.raw_bind_parameter(1,row.id)?;
            statement.raw_bind_parameter(2,row.tag)?;
            statement.raw_bind_parameter(3,row.cell)?;
            statement.raw_bind_parameter(4,row.world)?;
            statement.raw_bind_parameter(5,row.priority)?;
            statement.raw_bind_parameter(6,&row.data)?;
            Ok(())
        })();
        profile.record(BindSite::Bind,start,bound.is_ok(),row.data.len()); bound?;
        let start = profile.begin(); let executed=statement.raw_execute();
        profile.record(BindSite::ExecuteReset,start,executed.is_ok(),row.data.len());
        let changed=executed?;profile.sql_changes(changed);
        let start=profile.begin();statement.clear_bindings();
        profile.record(BindSite::Clear,start,true,row.data.len());profile.completed_row();Ok(changed)
    }

    fn snapshot(conn: &Connection) -> Vec<Vec<Value>> {
        let mut query=conn.prepare("SELECT form_id,typeof(form_id),record_type,typeof(record_type),cell_id,typeof(cell_id),worldspace_id,typeof(worldspace_id),load_order,typeof(load_order),data,typeof(data) FROM records ORDER BY form_id").unwrap();
        let rows=query.query_map([],|r| (0..12).map(|i|r.get(i)).collect()).unwrap();
        rows.collect::<Result<Vec<_>>>().unwrap()
    }

    #[test]
    fn uneven_values_null_zero_empty_blob_and_replacement_match_exactly() {
        for enabled in [false,true] {
            let mut left=database();let mut right=database();
            let rows=[Row{id:0xff123456,tag:"ACTI",cell:None,world:Some(0),priority:7,data:vec![]},
                Row{id:19,tag:"INFO",cell:Some(0xfedcba98),world:None,priority:u32::MAX,data:vec![0,255,7,19]},
                Row{id:0xff123456,tag:"UNKN",cell:Some(0),world:Some(0xffabcdef),priority:3,data:vec![23,0,1]}];
            let ltx=left.transaction().unwrap();let rtx=right.transaction().unwrap();
            let mut profile=CanonicalBindProfile::with_enabled(enabled);
            {
                let mut ls=ltx.prepare_cached(INSERT).unwrap();let mut rs=rtx.prepare_cached(INSERT).unwrap();
                for row in rows {assert_eq!(old(&mut ls,&row).unwrap(),new(&mut rs,&row,&mut profile).unwrap());}
            }
            ltx.commit().unwrap();rtx.commit().unwrap();profile.finish(true);
            assert_eq!(snapshot(&left),snapshot(&right));
            assert_eq!(right.query_row("SELECT count(*) FROM records WHERE form_id=19 AND worldspace_id IS NULL AND typeof(data)='blob'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
        }
    }

    #[test]
    fn empty_payload_is_blob_not_null_and_optional_zero_is_not_absent() {
        let conn=database();let mut statement=conn.prepare(INSERT).unwrap();let mut profile=CanonicalBindProfile::with_enabled(false);
        new(&mut statement,&Row{id:1,tag:"GLOB",cell:Some(0),world:None,priority:9,data:vec![]},&mut profile).unwrap();
        assert_eq!(conn.query_row("SELECT typeof(data),length(data),cell_id,worldspace_id IS NULL FROM records",[],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,bool>(3)?))).unwrap(),("blob".into(),0,0,true));
    }

    #[test]
    fn sql_constraint_error_and_failed_transaction_rollback_match() {
        let mut left=database();let mut right=database();let row=Row{id:8,tag:"STAT",cell:None,world:Some(5),priority:1,data:vec![7;32]};
        let ltx=left.transaction().unwrap();let rtx=right.transaction().unwrap();
        let mut profile=CanonicalBindProfile::with_enabled(false);
        {
            let mut ls=ltx.prepare_cached(INSERT).unwrap();let mut rs=rtx.prepare_cached(INSERT).unwrap();
            let le=old(&mut ls,&row).unwrap_err();let re=new(&mut rs,&row,&mut profile).unwrap_err();
            assert_eq!(le.sqlite_error_code(),re.sqlite_error_code());assert_eq!(le.to_string(),re.to_string());
        }
        drop(ltx);drop(rtx);assert!(snapshot(&left).is_empty());assert!(snapshot(&right).is_empty());
    }

    #[test]
    fn wrong_statement_cardinality_does_not_step_and_keeps_slice_diagnostic() {
        let conn=database();let row=Row{id:9,tag:"MATT",cell:None,world:None,priority:0,data:vec![4]};
        for sql in ["SELECT ?1,?2,?3","SELECT ?1,?2,?3,?4,?5","SELECT ?1,?2,?3,?4,?5,?6,?7"] {
            let mut ls=conn.prepare(sql).unwrap();let mut rs=conn.prepare(sql).unwrap();let mut profile=CanonicalBindProfile::with_enabled(false);
            let le=old(&mut ls,&row).unwrap_err();let re=new(&mut rs,&row,&mut profile).unwrap_err();
            assert_eq!(le.to_string(),re.to_string());assert!(matches!(re,Error::InvalidParameterCount(..)));
        }
        assert!(snapshot(&conn).is_empty());
    }

    struct BrokenValue;
    impl ToSql for BrokenValue {
        fn to_sql(&self)->Result<ToSqlOutput<'_>> {Err(Error::ToSqlConversionFailure(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData,"synthetic conversion failure"))))}
    }

    #[test]
    fn bind_conversion_failure_returns_before_execute_and_cached_drop_cleans() {
        let conn=database();
        {
            let mut ls=conn.prepare_cached(INSERT).unwrap();
            let le=ls.execute(params![1_u32,BrokenValue,None::<u32>,None::<u32>,0_u32,vec![7_u8]]).unwrap_err();
            drop(ls);
            let mut rs=conn.prepare_cached(INSERT).unwrap();rs.raw_bind_parameter(1,1_u32).unwrap();
            let re=rs.raw_bind_parameter(2,BrokenValue).unwrap_err();
            assert_eq!(le.to_string(),re.to_string());
        }
        assert!(snapshot(&conn).is_empty());
        let mut reused=conn.prepare_cached(INSERT).unwrap();let mut profile=CanonicalBindProfile::with_enabled(false);
        new(&mut reused,&Row{id:23,tag:"CAMS",cell:Some(4),world:None,priority:11,data:vec![19,2]},&mut profile).unwrap();
        assert_eq!(snapshot(&conn).len(),1);
    }
}
