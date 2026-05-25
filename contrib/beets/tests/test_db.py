import sqlite3

import pytest

from beetsplug._core import SchemaMismatch, check_schema_version, connect


def test_connect_and_version_ok(db_path):
    conn = connect(db_path)
    try:
        check_schema_version(conn)  # must not raise
    finally:
        conn.close()


def test_version_mismatch_raises(db_path):
    conn = sqlite3.connect(db_path)
    conn.execute("PRAGMA user_version = 2")
    conn.commit()
    conn.close()

    conn = connect(db_path)
    try:
        with pytest.raises(SchemaMismatch):
            check_schema_version(conn)
    finally:
        conn.close()
