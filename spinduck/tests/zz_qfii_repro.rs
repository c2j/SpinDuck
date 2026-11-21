#![cfg(feature = "native")]
use spinduck_core::{BindMap, DuckExecutor, Engine, RuleRegistry, SpinValue};

fn run(routine: &str) {
    let ddl = [
        "CREATE TABLE cur_src(id DECIMAL(10,0), tag VARCHAR)",
        "CREATE TABLE dim_sec(id DECIMAL(10,0), sec VARCHAR)",
        "CREATE TABLE dst_out(tag VARCHAR, sec VARCHAR)",
        "INSERT INTO cur_src VALUES (1,'a'),(2,'b')",
        "INSERT INTO dim_sec VALUES (1,'S1'),(2,'S2')",
    ];
    let mut eng = Engine::new(
        Box::new(DuckExecutor::open_in_memory().unwrap()),
        RuleRegistry::default(),
    );
    for sql in ddl {
        eng.ex.execute(sql, &BindMap::new()).unwrap();
    }
    eng.register(routine).unwrap();
    eng.call("p_test", &[SpinValue::Null]).unwrap();
}

#[test]
fn repro_inner_declare() {
    run("CREATE OR REPLACE PROCEDURE p_test(p_o OUT VARCHAR2) IS
      v_sec VARCHAR2(20);
    BEGIN
      p_o := '0';
      FOR r IN SELECT id, tag FROM cur_src LOOP
        DECLARE
          v_a VARCHAR2(20);
          v_sid VARCHAR2(20);
        BEGIN
          SELECT 'A' INTO v_a FROM dim_sec WHERE id = r.id;
          SELECT sec INTO v_sid FROM dim_sec WHERE id = r.id;
          INSERT INTO dst_out(tag, sec) VALUES (r.tag, v_sid);
        END;
      END LOOP;
    END;");
}

#[test]
fn repro_outer_declare() {
    run("CREATE OR REPLACE PROCEDURE p_test(p_o OUT VARCHAR2) IS
      v_sec VARCHAR2(20);
      v_a VARCHAR2(20);
      v_sid VARCHAR2(20);
    BEGIN
      p_o := '0';
      FOR r IN SELECT id, tag FROM cur_src LOOP
        BEGIN
          SELECT 'A' INTO v_a FROM dim_sec WHERE id = r.id;
          SELECT sec INTO v_sid FROM dim_sec WHERE id = r.id;
          INSERT INTO dst_out(tag, sec) VALUES (r.tag, v_sid);
        END;
      END LOOP;
    END;");
}

#[test]
fn repro_for_record_fields() {
    run("CREATE OR REPLACE PROCEDURE p_test(p_o OUT VARCHAR2) IS
    BEGIN
      p_o := '0';
      FOR r IN SELECT id, tag FROM cur_src LOOP
        INSERT INTO dst_out(tag, sec) VALUES (r.tag, r.tag);
      END LOOP;
    END;");
}

#[test]
fn repro_for_record_fields2() {
    run("CREATE OR REPLACE PROCEDURE p_test(p_o OUT VARCHAR2) IS
      v_sec VARCHAR2(20);
    BEGIN
      p_o := '0';
      FOR r IN SELECT id, tag FROM cur_src LOOP
        DECLARE
          v_sid VARCHAR2(20);
        BEGIN
          SELECT sec INTO v_sid FROM dim_sec WHERE id = r.id;
          INSERT INTO dst_out(tag, sec) VALUES (r.tag, v_sid);
        END;
      END LOOP;
    END;");
}
