//! The SQLSTATE codes of PostgreSQL, one constant for each code line of `errcodes.txt`.
//!
//! `cargo xtask errcodes` makes this file from `vendor/postgres-19/src/backend/utils/errcodes.txt`. Do not edit it.

use crate::sqlstate::{Category, SqlState};

impl SqlState {
    /// `00000`, condition `successful_completion`.
    pub const SUCCESSFUL_COMPLETION: Self = Self::from_bytes(*b"00000");
    /// `01000`, condition `warning`.
    pub const WARNING: Self = Self::from_bytes(*b"01000");
    /// `0100C`, condition `dynamic_result_sets_returned`.
    pub const WARNING_DYNAMIC_RESULT_SETS_RETURNED: Self = Self::from_bytes(*b"0100C");
    /// `01008`, condition `implicit_zero_bit_padding`.
    pub const WARNING_IMPLICIT_ZERO_BIT_PADDING: Self = Self::from_bytes(*b"01008");
    /// `01003`, condition `null_value_eliminated_in_set_function`.
    pub const WARNING_NULL_VALUE_ELIMINATED_IN_SET_FUNCTION: Self = Self::from_bytes(*b"01003");
    /// `01007`, condition `privilege_not_granted`.
    pub const WARNING_PRIVILEGE_NOT_GRANTED: Self = Self::from_bytes(*b"01007");
    /// `01006`, condition `privilege_not_revoked`.
    pub const WARNING_PRIVILEGE_NOT_REVOKED: Self = Self::from_bytes(*b"01006");
    /// `01004`, condition `string_data_right_truncation`.
    pub const WARNING_STRING_DATA_RIGHT_TRUNCATION: Self = Self::from_bytes(*b"01004");
    /// `01P01`, condition `deprecated_feature`.
    pub const WARNING_DEPRECATED_FEATURE: Self = Self::from_bytes(*b"01P01");
    /// `02000`, condition `no_data`.
    pub const NO_DATA: Self = Self::from_bytes(*b"02000");
    /// `02001`, condition `no_additional_dynamic_result_sets_returned`.
    pub const NO_ADDITIONAL_DYNAMIC_RESULT_SETS_RETURNED: Self = Self::from_bytes(*b"02001");
    /// `03000`, condition `sql_statement_not_yet_complete`.
    pub const SQL_STATEMENT_NOT_YET_COMPLETE: Self = Self::from_bytes(*b"03000");
    /// `08000`, condition `connection_exception`.
    pub const CONNECTION_EXCEPTION: Self = Self::from_bytes(*b"08000");
    /// `08003`, condition `connection_does_not_exist`.
    pub const CONNECTION_DOES_NOT_EXIST: Self = Self::from_bytes(*b"08003");
    /// `08006`, condition `connection_failure`.
    pub const CONNECTION_FAILURE: Self = Self::from_bytes(*b"08006");
    /// `08001`, condition `sqlclient_unable_to_establish_sqlconnection`.
    pub const SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION: Self = Self::from_bytes(*b"08001");
    /// `08004`, condition `sqlserver_rejected_establishment_of_sqlconnection`.
    pub const SQLSERVER_REJECTED_ESTABLISHMENT_OF_SQLCONNECTION: Self = Self::from_bytes(*b"08004");
    /// `08007`, condition `transaction_resolution_unknown`.
    pub const TRANSACTION_RESOLUTION_UNKNOWN: Self = Self::from_bytes(*b"08007");
    /// `08P01`, condition `protocol_violation`.
    pub const PROTOCOL_VIOLATION: Self = Self::from_bytes(*b"08P01");
    /// `09000`, condition `triggered_action_exception`.
    pub const TRIGGERED_ACTION_EXCEPTION: Self = Self::from_bytes(*b"09000");
    /// `0A000`, condition `feature_not_supported`.
    pub const FEATURE_NOT_SUPPORTED: Self = Self::from_bytes(*b"0A000");
    /// `0B000`, condition `invalid_transaction_initiation`.
    pub const INVALID_TRANSACTION_INITIATION: Self = Self::from_bytes(*b"0B000");
    /// `0F000`, condition `locator_exception`.
    pub const LOCATOR_EXCEPTION: Self = Self::from_bytes(*b"0F000");
    /// `0F001`, condition `invalid_locator_specification`.
    pub const L_E_INVALID_SPECIFICATION: Self = Self::from_bytes(*b"0F001");
    /// `0L000`, condition `invalid_grantor`.
    pub const INVALID_GRANTOR: Self = Self::from_bytes(*b"0L000");
    /// `0LP01`, condition `invalid_grant_operation`.
    pub const INVALID_GRANT_OPERATION: Self = Self::from_bytes(*b"0LP01");
    /// `0P000`, condition `invalid_role_specification`.
    pub const INVALID_ROLE_SPECIFICATION: Self = Self::from_bytes(*b"0P000");
    /// `0Z000`, condition `diagnostics_exception`.
    pub const DIAGNOSTICS_EXCEPTION: Self = Self::from_bytes(*b"0Z000");
    /// `0Z002`, condition `stacked_diagnostics_accessed_without_active_handler`.
    pub const STACKED_DIAGNOSTICS_ACCESSED_WITHOUT_ACTIVE_HANDLER: Self = Self::from_bytes(*b"0Z002");
    /// `10608`, condition `invalid_argument_for_xquery`.
    pub const INVALID_ARGUMENT_FOR_XQUERY: Self = Self::from_bytes(*b"10608");
    /// `20000`, condition `case_not_found`.
    pub const CASE_NOT_FOUND: Self = Self::from_bytes(*b"20000");
    /// `21000`, condition `cardinality_violation`.
    pub const CARDINALITY_VIOLATION: Self = Self::from_bytes(*b"21000");
    /// `22000`, condition `data_exception`.
    pub const DATA_EXCEPTION: Self = Self::from_bytes(*b"22000");
    /// `2202E`, a second name for this code.
    pub const ARRAY_ELEMENT_ERROR: Self = Self::from_bytes(*b"2202E");
    /// `2202E`, condition `array_subscript_error`.
    pub const ARRAY_SUBSCRIPT_ERROR: Self = Self::from_bytes(*b"2202E");
    /// `22021`, condition `character_not_in_repertoire`.
    pub const CHARACTER_NOT_IN_REPERTOIRE: Self = Self::from_bytes(*b"22021");
    /// `22008`, condition `datetime_field_overflow`.
    pub const DATETIME_FIELD_OVERFLOW: Self = Self::from_bytes(*b"22008");
    /// `22008`, a second name for this code.
    pub const DATETIME_VALUE_OUT_OF_RANGE: Self = Self::from_bytes(*b"22008");
    /// `22012`, condition `division_by_zero`.
    pub const DIVISION_BY_ZERO: Self = Self::from_bytes(*b"22012");
    /// `22005`, condition `error_in_assignment`.
    pub const ERROR_IN_ASSIGNMENT: Self = Self::from_bytes(*b"22005");
    /// `2200B`, condition `escape_character_conflict`.
    pub const ESCAPE_CHARACTER_CONFLICT: Self = Self::from_bytes(*b"2200B");
    /// `22022`, condition `indicator_overflow`.
    pub const INDICATOR_OVERFLOW: Self = Self::from_bytes(*b"22022");
    /// `22015`, condition `interval_field_overflow`.
    pub const INTERVAL_FIELD_OVERFLOW: Self = Self::from_bytes(*b"22015");
    /// `2201E`, condition `invalid_argument_for_logarithm`.
    pub const INVALID_ARGUMENT_FOR_LOG: Self = Self::from_bytes(*b"2201E");
    /// `22014`, condition `invalid_argument_for_ntile_function`.
    pub const INVALID_ARGUMENT_FOR_NTILE: Self = Self::from_bytes(*b"22014");
    /// `22016`, condition `invalid_argument_for_nth_value_function`.
    pub const INVALID_ARGUMENT_FOR_NTH_VALUE: Self = Self::from_bytes(*b"22016");
    /// `2201F`, condition `invalid_argument_for_power_function`.
    pub const INVALID_ARGUMENT_FOR_POWER_FUNCTION: Self = Self::from_bytes(*b"2201F");
    /// `2201G`, condition `invalid_argument_for_width_bucket_function`.
    pub const INVALID_ARGUMENT_FOR_WIDTH_BUCKET_FUNCTION: Self = Self::from_bytes(*b"2201G");
    /// `22018`, condition `invalid_character_value_for_cast`.
    pub const INVALID_CHARACTER_VALUE_FOR_CAST: Self = Self::from_bytes(*b"22018");
    /// `22007`, condition `invalid_datetime_format`.
    pub const INVALID_DATETIME_FORMAT: Self = Self::from_bytes(*b"22007");
    /// `22019`, condition `invalid_escape_character`.
    pub const INVALID_ESCAPE_CHARACTER: Self = Self::from_bytes(*b"22019");
    /// `2200D`, condition `invalid_escape_octet`.
    pub const INVALID_ESCAPE_OCTET: Self = Self::from_bytes(*b"2200D");
    /// `22025`, condition `invalid_escape_sequence`.
    pub const INVALID_ESCAPE_SEQUENCE: Self = Self::from_bytes(*b"22025");
    /// `22P06`, condition `nonstandard_use_of_escape_character`.
    pub const NONSTANDARD_USE_OF_ESCAPE_CHARACTER: Self = Self::from_bytes(*b"22P06");
    /// `22010`, condition `invalid_indicator_parameter_value`.
    pub const INVALID_INDICATOR_PARAMETER_VALUE: Self = Self::from_bytes(*b"22010");
    /// `22023`, condition `invalid_parameter_value`.
    pub const INVALID_PARAMETER_VALUE: Self = Self::from_bytes(*b"22023");
    /// `22013`, condition `invalid_preceding_or_following_size`.
    pub const INVALID_PRECEDING_OR_FOLLOWING_SIZE: Self = Self::from_bytes(*b"22013");
    /// `2201B`, condition `invalid_regular_expression`.
    pub const INVALID_REGULAR_EXPRESSION: Self = Self::from_bytes(*b"2201B");
    /// `2201W`, condition `invalid_row_count_in_limit_clause`.
    pub const INVALID_ROW_COUNT_IN_LIMIT_CLAUSE: Self = Self::from_bytes(*b"2201W");
    /// `2201X`, condition `invalid_row_count_in_result_offset_clause`.
    pub const INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE: Self = Self::from_bytes(*b"2201X");
    /// `2202H`, condition `invalid_tablesample_argument`.
    pub const INVALID_TABLESAMPLE_ARGUMENT: Self = Self::from_bytes(*b"2202H");
    /// `2202G`, condition `invalid_tablesample_repeat`.
    pub const INVALID_TABLESAMPLE_REPEAT: Self = Self::from_bytes(*b"2202G");
    /// `22009`, condition `invalid_time_zone_displacement_value`.
    pub const INVALID_TIME_ZONE_DISPLACEMENT_VALUE: Self = Self::from_bytes(*b"22009");
    /// `2200C`, condition `invalid_use_of_escape_character`.
    pub const INVALID_USE_OF_ESCAPE_CHARACTER: Self = Self::from_bytes(*b"2200C");
    /// `2200G`, condition `most_specific_type_mismatch`.
    pub const MOST_SPECIFIC_TYPE_MISMATCH: Self = Self::from_bytes(*b"2200G");
    /// `22004`, condition `null_value_not_allowed`.
    pub const NULL_VALUE_NOT_ALLOWED: Self = Self::from_bytes(*b"22004");
    /// `22002`, condition `null_value_no_indicator_parameter`.
    pub const NULL_VALUE_NO_INDICATOR_PARAMETER: Self = Self::from_bytes(*b"22002");
    /// `22003`, condition `numeric_value_out_of_range`.
    pub const NUMERIC_VALUE_OUT_OF_RANGE: Self = Self::from_bytes(*b"22003");
    /// `2200H`, condition `sequence_generator_limit_exceeded`.
    pub const SEQUENCE_GENERATOR_LIMIT_EXCEEDED: Self = Self::from_bytes(*b"2200H");
    /// `22026`, condition `string_data_length_mismatch`.
    pub const STRING_DATA_LENGTH_MISMATCH: Self = Self::from_bytes(*b"22026");
    /// `22001`, condition `string_data_right_truncation`.
    pub const STRING_DATA_RIGHT_TRUNCATION: Self = Self::from_bytes(*b"22001");
    /// `22011`, condition `substring_error`.
    pub const SUBSTRING_ERROR: Self = Self::from_bytes(*b"22011");
    /// `22027`, condition `trim_error`.
    pub const TRIM_ERROR: Self = Self::from_bytes(*b"22027");
    /// `22024`, condition `unterminated_c_string`.
    pub const UNTERMINATED_C_STRING: Self = Self::from_bytes(*b"22024");
    /// `2200F`, condition `zero_length_character_string`.
    pub const ZERO_LENGTH_CHARACTER_STRING: Self = Self::from_bytes(*b"2200F");
    /// `22P01`, condition `floating_point_exception`.
    pub const FLOATING_POINT_EXCEPTION: Self = Self::from_bytes(*b"22P01");
    /// `22P02`, condition `invalid_text_representation`.
    pub const INVALID_TEXT_REPRESENTATION: Self = Self::from_bytes(*b"22P02");
    /// `22P03`, condition `invalid_binary_representation`.
    pub const INVALID_BINARY_REPRESENTATION: Self = Self::from_bytes(*b"22P03");
    /// `22P04`, condition `bad_copy_file_format`.
    pub const BAD_COPY_FILE_FORMAT: Self = Self::from_bytes(*b"22P04");
    /// `22P05`, condition `untranslatable_character`.
    pub const UNTRANSLATABLE_CHARACTER: Self = Self::from_bytes(*b"22P05");
    /// `2200L`, condition `not_an_xml_document`.
    pub const NOT_AN_XML_DOCUMENT: Self = Self::from_bytes(*b"2200L");
    /// `2200M`, condition `invalid_xml_document`.
    pub const INVALID_XML_DOCUMENT: Self = Self::from_bytes(*b"2200M");
    /// `2200N`, condition `invalid_xml_content`.
    pub const INVALID_XML_CONTENT: Self = Self::from_bytes(*b"2200N");
    /// `2200S`, condition `invalid_xml_comment`.
    pub const INVALID_XML_COMMENT: Self = Self::from_bytes(*b"2200S");
    /// `2200T`, condition `invalid_xml_processing_instruction`.
    pub const INVALID_XML_PROCESSING_INSTRUCTION: Self = Self::from_bytes(*b"2200T");
    /// `22030`, condition `duplicate_json_object_key_value`.
    pub const DUPLICATE_JSON_OBJECT_KEY_VALUE: Self = Self::from_bytes(*b"22030");
    /// `22031`, condition `invalid_argument_for_sql_json_datetime_function`.
    pub const INVALID_ARGUMENT_FOR_SQL_JSON_DATETIME_FUNCTION: Self = Self::from_bytes(*b"22031");
    /// `22032`, condition `invalid_json_text`.
    pub const INVALID_JSON_TEXT: Self = Self::from_bytes(*b"22032");
    /// `22033`, condition `invalid_sql_json_subscript`.
    pub const INVALID_SQL_JSON_SUBSCRIPT: Self = Self::from_bytes(*b"22033");
    /// `22034`, condition `more_than_one_sql_json_item`.
    pub const MORE_THAN_ONE_SQL_JSON_ITEM: Self = Self::from_bytes(*b"22034");
    /// `22035`, condition `no_sql_json_item`.
    pub const NO_SQL_JSON_ITEM: Self = Self::from_bytes(*b"22035");
    /// `22036`, condition `non_numeric_sql_json_item`.
    pub const NON_NUMERIC_SQL_JSON_ITEM: Self = Self::from_bytes(*b"22036");
    /// `22037`, condition `non_unique_keys_in_a_json_object`.
    pub const NON_UNIQUE_KEYS_IN_A_JSON_OBJECT: Self = Self::from_bytes(*b"22037");
    /// `22038`, condition `singleton_sql_json_item_required`.
    pub const SINGLETON_SQL_JSON_ITEM_REQUIRED: Self = Self::from_bytes(*b"22038");
    /// `22039`, condition `sql_json_array_not_found`.
    pub const SQL_JSON_ARRAY_NOT_FOUND: Self = Self::from_bytes(*b"22039");
    /// `2203A`, condition `sql_json_member_not_found`.
    pub const SQL_JSON_MEMBER_NOT_FOUND: Self = Self::from_bytes(*b"2203A");
    /// `2203B`, condition `sql_json_number_not_found`.
    pub const SQL_JSON_NUMBER_NOT_FOUND: Self = Self::from_bytes(*b"2203B");
    /// `2203C`, condition `sql_json_object_not_found`.
    pub const SQL_JSON_OBJECT_NOT_FOUND: Self = Self::from_bytes(*b"2203C");
    /// `2203D`, condition `too_many_json_array_elements`.
    pub const TOO_MANY_JSON_ARRAY_ELEMENTS: Self = Self::from_bytes(*b"2203D");
    /// `2203E`, condition `too_many_json_object_members`.
    pub const TOO_MANY_JSON_OBJECT_MEMBERS: Self = Self::from_bytes(*b"2203E");
    /// `2203F`, condition `sql_json_scalar_required`.
    pub const SQL_JSON_SCALAR_REQUIRED: Self = Self::from_bytes(*b"2203F");
    /// `2203G`, condition `sql_json_item_cannot_be_cast_to_target_type`.
    pub const SQL_JSON_ITEM_CANNOT_BE_CAST_TO_TARGET_TYPE: Self = Self::from_bytes(*b"2203G");
    /// `23000`, condition `integrity_constraint_violation`.
    pub const INTEGRITY_CONSTRAINT_VIOLATION: Self = Self::from_bytes(*b"23000");
    /// `23001`, condition `restrict_violation`.
    pub const RESTRICT_VIOLATION: Self = Self::from_bytes(*b"23001");
    /// `23502`, condition `not_null_violation`.
    pub const NOT_NULL_VIOLATION: Self = Self::from_bytes(*b"23502");
    /// `23503`, condition `foreign_key_violation`.
    pub const FOREIGN_KEY_VIOLATION: Self = Self::from_bytes(*b"23503");
    /// `23505`, condition `unique_violation`.
    pub const UNIQUE_VIOLATION: Self = Self::from_bytes(*b"23505");
    /// `23514`, condition `check_violation`.
    pub const CHECK_VIOLATION: Self = Self::from_bytes(*b"23514");
    /// `23P01`, condition `exclusion_violation`.
    pub const EXCLUSION_VIOLATION: Self = Self::from_bytes(*b"23P01");
    /// `24000`, condition `invalid_cursor_state`.
    pub const INVALID_CURSOR_STATE: Self = Self::from_bytes(*b"24000");
    /// `25000`, condition `invalid_transaction_state`.
    pub const INVALID_TRANSACTION_STATE: Self = Self::from_bytes(*b"25000");
    /// `25001`, condition `active_sql_transaction`.
    pub const ACTIVE_SQL_TRANSACTION: Self = Self::from_bytes(*b"25001");
    /// `25002`, condition `branch_transaction_already_active`.
    pub const BRANCH_TRANSACTION_ALREADY_ACTIVE: Self = Self::from_bytes(*b"25002");
    /// `25008`, condition `held_cursor_requires_same_isolation_level`.
    pub const HELD_CURSOR_REQUIRES_SAME_ISOLATION_LEVEL: Self = Self::from_bytes(*b"25008");
    /// `25003`, condition `inappropriate_access_mode_for_branch_transaction`.
    pub const INAPPROPRIATE_ACCESS_MODE_FOR_BRANCH_TRANSACTION: Self = Self::from_bytes(*b"25003");
    /// `25004`, condition `inappropriate_isolation_level_for_branch_transaction`.
    pub const INAPPROPRIATE_ISOLATION_LEVEL_FOR_BRANCH_TRANSACTION: Self = Self::from_bytes(*b"25004");
    /// `25005`, condition `no_active_sql_transaction_for_branch_transaction`.
    pub const NO_ACTIVE_SQL_TRANSACTION_FOR_BRANCH_TRANSACTION: Self = Self::from_bytes(*b"25005");
    /// `25006`, condition `read_only_sql_transaction`.
    pub const READ_ONLY_SQL_TRANSACTION: Self = Self::from_bytes(*b"25006");
    /// `25007`, condition `schema_and_data_statement_mixing_not_supported`.
    pub const SCHEMA_AND_DATA_STATEMENT_MIXING_NOT_SUPPORTED: Self = Self::from_bytes(*b"25007");
    /// `25P01`, condition `no_active_sql_transaction`.
    pub const NO_ACTIVE_SQL_TRANSACTION: Self = Self::from_bytes(*b"25P01");
    /// `25P02`, condition `in_failed_sql_transaction`.
    pub const IN_FAILED_SQL_TRANSACTION: Self = Self::from_bytes(*b"25P02");
    /// `25P03`, condition `idle_in_transaction_session_timeout`.
    pub const IDLE_IN_TRANSACTION_SESSION_TIMEOUT: Self = Self::from_bytes(*b"25P03");
    /// `25P04`, condition `transaction_timeout`.
    pub const TRANSACTION_TIMEOUT: Self = Self::from_bytes(*b"25P04");
    /// `26000`, condition `invalid_sql_statement_name`.
    pub const INVALID_SQL_STATEMENT_NAME: Self = Self::from_bytes(*b"26000");
    /// `27000`, condition `triggered_data_change_violation`.
    pub const TRIGGERED_DATA_CHANGE_VIOLATION: Self = Self::from_bytes(*b"27000");
    /// `28000`, condition `invalid_authorization_specification`.
    pub const INVALID_AUTHORIZATION_SPECIFICATION: Self = Self::from_bytes(*b"28000");
    /// `28P01`, condition `invalid_password`.
    pub const INVALID_PASSWORD: Self = Self::from_bytes(*b"28P01");
    /// `2B000`, condition `dependent_privilege_descriptors_still_exist`.
    pub const DEPENDENT_PRIVILEGE_DESCRIPTORS_STILL_EXIST: Self = Self::from_bytes(*b"2B000");
    /// `2BP01`, condition `dependent_objects_still_exist`.
    pub const DEPENDENT_OBJECTS_STILL_EXIST: Self = Self::from_bytes(*b"2BP01");
    /// `2D000`, condition `invalid_transaction_termination`.
    pub const INVALID_TRANSACTION_TERMINATION: Self = Self::from_bytes(*b"2D000");
    /// `2F000`, condition `sql_routine_exception`.
    pub const SQL_ROUTINE_EXCEPTION: Self = Self::from_bytes(*b"2F000");
    /// `2F005`, condition `function_executed_no_return_statement`.
    pub const S_R_E_FUNCTION_EXECUTED_NO_RETURN_STATEMENT: Self = Self::from_bytes(*b"2F005");
    /// `2F002`, condition `modifying_sql_data_not_permitted`.
    pub const S_R_E_MODIFYING_SQL_DATA_NOT_PERMITTED: Self = Self::from_bytes(*b"2F002");
    /// `2F003`, condition `prohibited_sql_statement_attempted`.
    pub const S_R_E_PROHIBITED_SQL_STATEMENT_ATTEMPTED: Self = Self::from_bytes(*b"2F003");
    /// `2F004`, condition `reading_sql_data_not_permitted`.
    pub const S_R_E_READING_SQL_DATA_NOT_PERMITTED: Self = Self::from_bytes(*b"2F004");
    /// `34000`, condition `invalid_cursor_name`.
    pub const INVALID_CURSOR_NAME: Self = Self::from_bytes(*b"34000");
    /// `38000`, condition `external_routine_exception`.
    pub const EXTERNAL_ROUTINE_EXCEPTION: Self = Self::from_bytes(*b"38000");
    /// `38001`, condition `containing_sql_not_permitted`.
    pub const E_R_E_CONTAINING_SQL_NOT_PERMITTED: Self = Self::from_bytes(*b"38001");
    /// `38002`, condition `modifying_sql_data_not_permitted`.
    pub const E_R_E_MODIFYING_SQL_DATA_NOT_PERMITTED: Self = Self::from_bytes(*b"38002");
    /// `38003`, condition `prohibited_sql_statement_attempted`.
    pub const E_R_E_PROHIBITED_SQL_STATEMENT_ATTEMPTED: Self = Self::from_bytes(*b"38003");
    /// `38004`, condition `reading_sql_data_not_permitted`.
    pub const E_R_E_READING_SQL_DATA_NOT_PERMITTED: Self = Self::from_bytes(*b"38004");
    /// `39000`, condition `external_routine_invocation_exception`.
    pub const EXTERNAL_ROUTINE_INVOCATION_EXCEPTION: Self = Self::from_bytes(*b"39000");
    /// `39001`, condition `invalid_sqlstate_returned`.
    pub const E_R_I_E_INVALID_SQLSTATE_RETURNED: Self = Self::from_bytes(*b"39001");
    /// `39004`, condition `null_value_not_allowed`.
    pub const E_R_I_E_NULL_VALUE_NOT_ALLOWED: Self = Self::from_bytes(*b"39004");
    /// `39P01`, condition `trigger_protocol_violated`.
    pub const E_R_I_E_TRIGGER_PROTOCOL_VIOLATED: Self = Self::from_bytes(*b"39P01");
    /// `39P02`, condition `srf_protocol_violated`.
    pub const E_R_I_E_SRF_PROTOCOL_VIOLATED: Self = Self::from_bytes(*b"39P02");
    /// `39P03`, condition `event_trigger_protocol_violated`.
    pub const E_R_I_E_EVENT_TRIGGER_PROTOCOL_VIOLATED: Self = Self::from_bytes(*b"39P03");
    /// `3B000`, condition `savepoint_exception`.
    pub const SAVEPOINT_EXCEPTION: Self = Self::from_bytes(*b"3B000");
    /// `3B001`, condition `invalid_savepoint_specification`.
    pub const S_E_INVALID_SPECIFICATION: Self = Self::from_bytes(*b"3B001");
    /// `3D000`, condition `invalid_catalog_name`.
    pub const INVALID_CATALOG_NAME: Self = Self::from_bytes(*b"3D000");
    /// `3F000`, condition `invalid_schema_name`.
    pub const INVALID_SCHEMA_NAME: Self = Self::from_bytes(*b"3F000");
    /// `40000`, condition `transaction_rollback`.
    pub const TRANSACTION_ROLLBACK: Self = Self::from_bytes(*b"40000");
    /// `40002`, condition `transaction_integrity_constraint_violation`.
    pub const T_R_INTEGRITY_CONSTRAINT_VIOLATION: Self = Self::from_bytes(*b"40002");
    /// `40001`, condition `serialization_failure`.
    pub const T_R_SERIALIZATION_FAILURE: Self = Self::from_bytes(*b"40001");
    /// `40003`, condition `statement_completion_unknown`.
    pub const T_R_STATEMENT_COMPLETION_UNKNOWN: Self = Self::from_bytes(*b"40003");
    /// `40P01`, condition `deadlock_detected`.
    pub const T_R_DEADLOCK_DETECTED: Self = Self::from_bytes(*b"40P01");
    /// `42000`, condition `syntax_error_or_access_rule_violation`.
    pub const SYNTAX_ERROR_OR_ACCESS_RULE_VIOLATION: Self = Self::from_bytes(*b"42000");
    /// `42601`, condition `syntax_error`.
    pub const SYNTAX_ERROR: Self = Self::from_bytes(*b"42601");
    /// `42501`, condition `insufficient_privilege`.
    pub const INSUFFICIENT_PRIVILEGE: Self = Self::from_bytes(*b"42501");
    /// `42846`, condition `cannot_coerce`.
    pub const CANNOT_COERCE: Self = Self::from_bytes(*b"42846");
    /// `42803`, condition `grouping_error`.
    pub const GROUPING_ERROR: Self = Self::from_bytes(*b"42803");
    /// `42P20`, condition `windowing_error`.
    pub const WINDOWING_ERROR: Self = Self::from_bytes(*b"42P20");
    /// `42P19`, condition `invalid_recursion`.
    pub const INVALID_RECURSION: Self = Self::from_bytes(*b"42P19");
    /// `42830`, condition `invalid_foreign_key`.
    pub const INVALID_FOREIGN_KEY: Self = Self::from_bytes(*b"42830");
    /// `42602`, condition `invalid_name`.
    pub const INVALID_NAME: Self = Self::from_bytes(*b"42602");
    /// `42622`, condition `name_too_long`.
    pub const NAME_TOO_LONG: Self = Self::from_bytes(*b"42622");
    /// `42939`, condition `reserved_name`.
    pub const RESERVED_NAME: Self = Self::from_bytes(*b"42939");
    /// `42804`, condition `datatype_mismatch`.
    pub const DATATYPE_MISMATCH: Self = Self::from_bytes(*b"42804");
    /// `42P18`, condition `indeterminate_datatype`.
    pub const INDETERMINATE_DATATYPE: Self = Self::from_bytes(*b"42P18");
    /// `42P21`, condition `collation_mismatch`.
    pub const COLLATION_MISMATCH: Self = Self::from_bytes(*b"42P21");
    /// `42P22`, condition `indeterminate_collation`.
    pub const INDETERMINATE_COLLATION: Self = Self::from_bytes(*b"42P22");
    /// `42809`, condition `wrong_object_type`.
    pub const WRONG_OBJECT_TYPE: Self = Self::from_bytes(*b"42809");
    /// `428C9`, condition `generated_always`.
    pub const GENERATED_ALWAYS: Self = Self::from_bytes(*b"428C9");
    /// `42703`, condition `undefined_column`.
    pub const UNDEFINED_COLUMN: Self = Self::from_bytes(*b"42703");
    /// `34000`, a second name for this code.
    pub const UNDEFINED_CURSOR: Self = Self::from_bytes(*b"34000");
    /// `3D000`, a second name for this code.
    pub const UNDEFINED_DATABASE: Self = Self::from_bytes(*b"3D000");
    /// `42883`, condition `undefined_function`.
    pub const UNDEFINED_FUNCTION: Self = Self::from_bytes(*b"42883");
    /// `26000`, a second name for this code.
    pub const UNDEFINED_PSTATEMENT: Self = Self::from_bytes(*b"26000");
    /// `3F000`, a second name for this code.
    pub const UNDEFINED_SCHEMA: Self = Self::from_bytes(*b"3F000");
    /// `42P01`, condition `undefined_table`.
    pub const UNDEFINED_TABLE: Self = Self::from_bytes(*b"42P01");
    /// `42P02`, condition `undefined_parameter`.
    pub const UNDEFINED_PARAMETER: Self = Self::from_bytes(*b"42P02");
    /// `42704`, condition `undefined_object`.
    pub const UNDEFINED_OBJECT: Self = Self::from_bytes(*b"42704");
    /// `42701`, condition `duplicate_column`.
    pub const DUPLICATE_COLUMN: Self = Self::from_bytes(*b"42701");
    /// `42P03`, condition `duplicate_cursor`.
    pub const DUPLICATE_CURSOR: Self = Self::from_bytes(*b"42P03");
    /// `42P04`, condition `duplicate_database`.
    pub const DUPLICATE_DATABASE: Self = Self::from_bytes(*b"42P04");
    /// `42723`, condition `duplicate_function`.
    pub const DUPLICATE_FUNCTION: Self = Self::from_bytes(*b"42723");
    /// `42P05`, condition `duplicate_prepared_statement`.
    pub const DUPLICATE_PSTATEMENT: Self = Self::from_bytes(*b"42P05");
    /// `42P06`, condition `duplicate_schema`.
    pub const DUPLICATE_SCHEMA: Self = Self::from_bytes(*b"42P06");
    /// `42P07`, condition `duplicate_table`.
    pub const DUPLICATE_TABLE: Self = Self::from_bytes(*b"42P07");
    /// `42712`, condition `duplicate_alias`.
    pub const DUPLICATE_ALIAS: Self = Self::from_bytes(*b"42712");
    /// `42710`, condition `duplicate_object`.
    pub const DUPLICATE_OBJECT: Self = Self::from_bytes(*b"42710");
    /// `42702`, condition `ambiguous_column`.
    pub const AMBIGUOUS_COLUMN: Self = Self::from_bytes(*b"42702");
    /// `42725`, condition `ambiguous_function`.
    pub const AMBIGUOUS_FUNCTION: Self = Self::from_bytes(*b"42725");
    /// `42P08`, condition `ambiguous_parameter`.
    pub const AMBIGUOUS_PARAMETER: Self = Self::from_bytes(*b"42P08");
    /// `42P09`, condition `ambiguous_alias`.
    pub const AMBIGUOUS_ALIAS: Self = Self::from_bytes(*b"42P09");
    /// `42P10`, condition `invalid_column_reference`.
    pub const INVALID_COLUMN_REFERENCE: Self = Self::from_bytes(*b"42P10");
    /// `42611`, condition `invalid_column_definition`.
    pub const INVALID_COLUMN_DEFINITION: Self = Self::from_bytes(*b"42611");
    /// `42P11`, condition `invalid_cursor_definition`.
    pub const INVALID_CURSOR_DEFINITION: Self = Self::from_bytes(*b"42P11");
    /// `42P12`, condition `invalid_database_definition`.
    pub const INVALID_DATABASE_DEFINITION: Self = Self::from_bytes(*b"42P12");
    /// `42P13`, condition `invalid_function_definition`.
    pub const INVALID_FUNCTION_DEFINITION: Self = Self::from_bytes(*b"42P13");
    /// `42P14`, condition `invalid_prepared_statement_definition`.
    pub const INVALID_PSTATEMENT_DEFINITION: Self = Self::from_bytes(*b"42P14");
    /// `42P15`, condition `invalid_schema_definition`.
    pub const INVALID_SCHEMA_DEFINITION: Self = Self::from_bytes(*b"42P15");
    /// `42P16`, condition `invalid_table_definition`.
    pub const INVALID_TABLE_DEFINITION: Self = Self::from_bytes(*b"42P16");
    /// `42P17`, condition `invalid_object_definition`.
    pub const INVALID_OBJECT_DEFINITION: Self = Self::from_bytes(*b"42P17");
    /// `44000`, condition `with_check_option_violation`.
    pub const WITH_CHECK_OPTION_VIOLATION: Self = Self::from_bytes(*b"44000");
    /// `53000`, condition `insufficient_resources`.
    pub const INSUFFICIENT_RESOURCES: Self = Self::from_bytes(*b"53000");
    /// `53100`, condition `disk_full`.
    pub const DISK_FULL: Self = Self::from_bytes(*b"53100");
    /// `53200`, condition `out_of_memory`.
    pub const OUT_OF_MEMORY: Self = Self::from_bytes(*b"53200");
    /// `53300`, condition `too_many_connections`.
    pub const TOO_MANY_CONNECTIONS: Self = Self::from_bytes(*b"53300");
    /// `53400`, condition `configuration_limit_exceeded`.
    pub const CONFIGURATION_LIMIT_EXCEEDED: Self = Self::from_bytes(*b"53400");
    /// `54000`, condition `program_limit_exceeded`.
    pub const PROGRAM_LIMIT_EXCEEDED: Self = Self::from_bytes(*b"54000");
    /// `54001`, condition `statement_too_complex`.
    pub const STATEMENT_TOO_COMPLEX: Self = Self::from_bytes(*b"54001");
    /// `54011`, condition `too_many_columns`.
    pub const TOO_MANY_COLUMNS: Self = Self::from_bytes(*b"54011");
    /// `54023`, condition `too_many_arguments`.
    pub const TOO_MANY_ARGUMENTS: Self = Self::from_bytes(*b"54023");
    /// `55000`, condition `object_not_in_prerequisite_state`.
    pub const OBJECT_NOT_IN_PREREQUISITE_STATE: Self = Self::from_bytes(*b"55000");
    /// `55006`, condition `object_in_use`.
    pub const OBJECT_IN_USE: Self = Self::from_bytes(*b"55006");
    /// `55P02`, condition `cant_change_runtime_param`.
    pub const CANT_CHANGE_RUNTIME_PARAM: Self = Self::from_bytes(*b"55P02");
    /// `55P03`, condition `lock_not_available`.
    pub const LOCK_NOT_AVAILABLE: Self = Self::from_bytes(*b"55P03");
    /// `55P04`, condition `unsafe_new_enum_value_usage`.
    pub const UNSAFE_NEW_ENUM_VALUE_USAGE: Self = Self::from_bytes(*b"55P04");
    /// `57000`, condition `operator_intervention`.
    pub const OPERATOR_INTERVENTION: Self = Self::from_bytes(*b"57000");
    /// `57014`, condition `query_canceled`.
    pub const QUERY_CANCELED: Self = Self::from_bytes(*b"57014");
    /// `57P01`, condition `admin_shutdown`.
    pub const ADMIN_SHUTDOWN: Self = Self::from_bytes(*b"57P01");
    /// `57P02`, condition `crash_shutdown`.
    pub const CRASH_SHUTDOWN: Self = Self::from_bytes(*b"57P02");
    /// `57P03`, condition `cannot_connect_now`.
    pub const CANNOT_CONNECT_NOW: Self = Self::from_bytes(*b"57P03");
    /// `57P04`, condition `database_dropped`.
    pub const DATABASE_DROPPED: Self = Self::from_bytes(*b"57P04");
    /// `57P05`, condition `idle_session_timeout`.
    pub const IDLE_SESSION_TIMEOUT: Self = Self::from_bytes(*b"57P05");
    /// `58000`, condition `system_error`.
    pub const SYSTEM_ERROR: Self = Self::from_bytes(*b"58000");
    /// `58030`, condition `io_error`.
    pub const IO_ERROR: Self = Self::from_bytes(*b"58030");
    /// `58P01`, condition `undefined_file`.
    pub const UNDEFINED_FILE: Self = Self::from_bytes(*b"58P01");
    /// `58P02`, condition `duplicate_file`.
    pub const DUPLICATE_FILE: Self = Self::from_bytes(*b"58P02");
    /// `58P03`, condition `file_name_too_long`.
    pub const FILE_NAME_TOO_LONG: Self = Self::from_bytes(*b"58P03");
    /// `F0000`, condition `config_file_error`.
    pub const CONFIG_FILE_ERROR: Self = Self::from_bytes(*b"F0000");
    /// `F0001`, condition `lock_file_exists`.
    pub const LOCK_FILE_EXISTS: Self = Self::from_bytes(*b"F0001");
    /// `HV000`, condition `fdw_error`.
    pub const FDW_ERROR: Self = Self::from_bytes(*b"HV000");
    /// `HV005`, condition `fdw_column_name_not_found`.
    pub const FDW_COLUMN_NAME_NOT_FOUND: Self = Self::from_bytes(*b"HV005");
    /// `HV002`, condition `fdw_dynamic_parameter_value_needed`.
    pub const FDW_DYNAMIC_PARAMETER_VALUE_NEEDED: Self = Self::from_bytes(*b"HV002");
    /// `HV010`, condition `fdw_function_sequence_error`.
    pub const FDW_FUNCTION_SEQUENCE_ERROR: Self = Self::from_bytes(*b"HV010");
    /// `HV021`, condition `fdw_inconsistent_descriptor_information`.
    pub const FDW_INCONSISTENT_DESCRIPTOR_INFORMATION: Self = Self::from_bytes(*b"HV021");
    /// `HV024`, condition `fdw_invalid_attribute_value`.
    pub const FDW_INVALID_ATTRIBUTE_VALUE: Self = Self::from_bytes(*b"HV024");
    /// `HV007`, condition `fdw_invalid_column_name`.
    pub const FDW_INVALID_COLUMN_NAME: Self = Self::from_bytes(*b"HV007");
    /// `HV008`, condition `fdw_invalid_column_number`.
    pub const FDW_INVALID_COLUMN_NUMBER: Self = Self::from_bytes(*b"HV008");
    /// `HV004`, condition `fdw_invalid_data_type`.
    pub const FDW_INVALID_DATA_TYPE: Self = Self::from_bytes(*b"HV004");
    /// `HV006`, condition `fdw_invalid_data_type_descriptors`.
    pub const FDW_INVALID_DATA_TYPE_DESCRIPTORS: Self = Self::from_bytes(*b"HV006");
    /// `HV091`, condition `fdw_invalid_descriptor_field_identifier`.
    pub const FDW_INVALID_DESCRIPTOR_FIELD_IDENTIFIER: Self = Self::from_bytes(*b"HV091");
    /// `HV00B`, condition `fdw_invalid_handle`.
    pub const FDW_INVALID_HANDLE: Self = Self::from_bytes(*b"HV00B");
    /// `HV00C`, condition `fdw_invalid_option_index`.
    pub const FDW_INVALID_OPTION_INDEX: Self = Self::from_bytes(*b"HV00C");
    /// `HV00D`, condition `fdw_invalid_option_name`.
    pub const FDW_INVALID_OPTION_NAME: Self = Self::from_bytes(*b"HV00D");
    /// `HV090`, condition `fdw_invalid_string_length_or_buffer_length`.
    pub const FDW_INVALID_STRING_LENGTH_OR_BUFFER_LENGTH: Self = Self::from_bytes(*b"HV090");
    /// `HV00A`, condition `fdw_invalid_string_format`.
    pub const FDW_INVALID_STRING_FORMAT: Self = Self::from_bytes(*b"HV00A");
    /// `HV009`, condition `fdw_invalid_use_of_null_pointer`.
    pub const FDW_INVALID_USE_OF_NULL_POINTER: Self = Self::from_bytes(*b"HV009");
    /// `HV014`, condition `fdw_too_many_handles`.
    pub const FDW_TOO_MANY_HANDLES: Self = Self::from_bytes(*b"HV014");
    /// `HV001`, condition `fdw_out_of_memory`.
    pub const FDW_OUT_OF_MEMORY: Self = Self::from_bytes(*b"HV001");
    /// `HV00P`, condition `fdw_no_schemas`.
    pub const FDW_NO_SCHEMAS: Self = Self::from_bytes(*b"HV00P");
    /// `HV00J`, condition `fdw_option_name_not_found`.
    pub const FDW_OPTION_NAME_NOT_FOUND: Self = Self::from_bytes(*b"HV00J");
    /// `HV00K`, condition `fdw_reply_handle`.
    pub const FDW_REPLY_HANDLE: Self = Self::from_bytes(*b"HV00K");
    /// `HV00Q`, condition `fdw_schema_not_found`.
    pub const FDW_SCHEMA_NOT_FOUND: Self = Self::from_bytes(*b"HV00Q");
    /// `HV00R`, condition `fdw_table_not_found`.
    pub const FDW_TABLE_NOT_FOUND: Self = Self::from_bytes(*b"HV00R");
    /// `HV00L`, condition `fdw_unable_to_create_execution`.
    pub const FDW_UNABLE_TO_CREATE_EXECUTION: Self = Self::from_bytes(*b"HV00L");
    /// `HV00M`, condition `fdw_unable_to_create_reply`.
    pub const FDW_UNABLE_TO_CREATE_REPLY: Self = Self::from_bytes(*b"HV00M");
    /// `HV00N`, condition `fdw_unable_to_establish_connection`.
    pub const FDW_UNABLE_TO_ESTABLISH_CONNECTION: Self = Self::from_bytes(*b"HV00N");
    /// `P0000`, condition `plpgsql_error`.
    pub const PLPGSQL_ERROR: Self = Self::from_bytes(*b"P0000");
    /// `P0001`, condition `raise_exception`.
    pub const RAISE_EXCEPTION: Self = Self::from_bytes(*b"P0001");
    /// `P0002`, condition `no_data_found`.
    pub const NO_DATA_FOUND: Self = Self::from_bytes(*b"P0002");
    /// `P0003`, condition `too_many_rows`.
    pub const TOO_MANY_ROWS: Self = Self::from_bytes(*b"P0003");
    /// `P0004`, condition `assert_failure`.
    pub const ASSERT_FAILURE: Self = Self::from_bytes(*b"P0004");
    /// `XX000`, condition `internal_error`.
    pub const INTERNAL_ERROR: Self = Self::from_bytes(*b"XX000");
    /// `XX001`, condition `data_corrupted`.
    pub const DATA_CORRUPTED: Self = Self::from_bytes(*b"XX001");
    /// `XX002`, condition `index_corrupted`.
    pub const INDEX_CORRUPTED: Self = Self::from_bytes(*b"XX002");
}

/// Every code line of the file, in file order: the code, the category, the name of the constant and the condition name. The condition name is empty for a second name.
pub(crate) static CODES: [(SqlState, Category, &str, &str); 268] = [
    (SqlState::SUCCESSFUL_COMPLETION, Category::Success, "SUCCESSFUL_COMPLETION", "successful_completion"),
    (SqlState::WARNING, Category::Warning, "WARNING", "warning"),
    (SqlState::WARNING_DYNAMIC_RESULT_SETS_RETURNED, Category::Warning, "WARNING_DYNAMIC_RESULT_SETS_RETURNED", "dynamic_result_sets_returned"),
    (SqlState::WARNING_IMPLICIT_ZERO_BIT_PADDING, Category::Warning, "WARNING_IMPLICIT_ZERO_BIT_PADDING", "implicit_zero_bit_padding"),
    (SqlState::WARNING_NULL_VALUE_ELIMINATED_IN_SET_FUNCTION, Category::Warning, "WARNING_NULL_VALUE_ELIMINATED_IN_SET_FUNCTION", "null_value_eliminated_in_set_function"),
    (SqlState::WARNING_PRIVILEGE_NOT_GRANTED, Category::Warning, "WARNING_PRIVILEGE_NOT_GRANTED", "privilege_not_granted"),
    (SqlState::WARNING_PRIVILEGE_NOT_REVOKED, Category::Warning, "WARNING_PRIVILEGE_NOT_REVOKED", "privilege_not_revoked"),
    (SqlState::WARNING_STRING_DATA_RIGHT_TRUNCATION, Category::Warning, "WARNING_STRING_DATA_RIGHT_TRUNCATION", "string_data_right_truncation"),
    (SqlState::WARNING_DEPRECATED_FEATURE, Category::Warning, "WARNING_DEPRECATED_FEATURE", "deprecated_feature"),
    (SqlState::NO_DATA, Category::Warning, "NO_DATA", "no_data"),
    (SqlState::NO_ADDITIONAL_DYNAMIC_RESULT_SETS_RETURNED, Category::Warning, "NO_ADDITIONAL_DYNAMIC_RESULT_SETS_RETURNED", "no_additional_dynamic_result_sets_returned"),
    (SqlState::SQL_STATEMENT_NOT_YET_COMPLETE, Category::Error, "SQL_STATEMENT_NOT_YET_COMPLETE", "sql_statement_not_yet_complete"),
    (SqlState::CONNECTION_EXCEPTION, Category::Error, "CONNECTION_EXCEPTION", "connection_exception"),
    (SqlState::CONNECTION_DOES_NOT_EXIST, Category::Error, "CONNECTION_DOES_NOT_EXIST", "connection_does_not_exist"),
    (SqlState::CONNECTION_FAILURE, Category::Error, "CONNECTION_FAILURE", "connection_failure"),
    (SqlState::SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION, Category::Error, "SQLCLIENT_UNABLE_TO_ESTABLISH_SQLCONNECTION", "sqlclient_unable_to_establish_sqlconnection"),
    (SqlState::SQLSERVER_REJECTED_ESTABLISHMENT_OF_SQLCONNECTION, Category::Error, "SQLSERVER_REJECTED_ESTABLISHMENT_OF_SQLCONNECTION", "sqlserver_rejected_establishment_of_sqlconnection"),
    (SqlState::TRANSACTION_RESOLUTION_UNKNOWN, Category::Error, "TRANSACTION_RESOLUTION_UNKNOWN", "transaction_resolution_unknown"),
    (SqlState::PROTOCOL_VIOLATION, Category::Error, "PROTOCOL_VIOLATION", "protocol_violation"),
    (SqlState::TRIGGERED_ACTION_EXCEPTION, Category::Error, "TRIGGERED_ACTION_EXCEPTION", "triggered_action_exception"),
    (SqlState::FEATURE_NOT_SUPPORTED, Category::Error, "FEATURE_NOT_SUPPORTED", "feature_not_supported"),
    (SqlState::INVALID_TRANSACTION_INITIATION, Category::Error, "INVALID_TRANSACTION_INITIATION", "invalid_transaction_initiation"),
    (SqlState::LOCATOR_EXCEPTION, Category::Error, "LOCATOR_EXCEPTION", "locator_exception"),
    (SqlState::L_E_INVALID_SPECIFICATION, Category::Error, "L_E_INVALID_SPECIFICATION", "invalid_locator_specification"),
    (SqlState::INVALID_GRANTOR, Category::Error, "INVALID_GRANTOR", "invalid_grantor"),
    (SqlState::INVALID_GRANT_OPERATION, Category::Error, "INVALID_GRANT_OPERATION", "invalid_grant_operation"),
    (SqlState::INVALID_ROLE_SPECIFICATION, Category::Error, "INVALID_ROLE_SPECIFICATION", "invalid_role_specification"),
    (SqlState::DIAGNOSTICS_EXCEPTION, Category::Error, "DIAGNOSTICS_EXCEPTION", "diagnostics_exception"),
    (SqlState::STACKED_DIAGNOSTICS_ACCESSED_WITHOUT_ACTIVE_HANDLER, Category::Error, "STACKED_DIAGNOSTICS_ACCESSED_WITHOUT_ACTIVE_HANDLER", "stacked_diagnostics_accessed_without_active_handler"),
    (SqlState::INVALID_ARGUMENT_FOR_XQUERY, Category::Error, "INVALID_ARGUMENT_FOR_XQUERY", "invalid_argument_for_xquery"),
    (SqlState::CASE_NOT_FOUND, Category::Error, "CASE_NOT_FOUND", "case_not_found"),
    (SqlState::CARDINALITY_VIOLATION, Category::Error, "CARDINALITY_VIOLATION", "cardinality_violation"),
    (SqlState::DATA_EXCEPTION, Category::Error, "DATA_EXCEPTION", "data_exception"),
    (SqlState::ARRAY_ELEMENT_ERROR, Category::Error, "ARRAY_ELEMENT_ERROR", ""),
    (SqlState::ARRAY_SUBSCRIPT_ERROR, Category::Error, "ARRAY_SUBSCRIPT_ERROR", "array_subscript_error"),
    (SqlState::CHARACTER_NOT_IN_REPERTOIRE, Category::Error, "CHARACTER_NOT_IN_REPERTOIRE", "character_not_in_repertoire"),
    (SqlState::DATETIME_FIELD_OVERFLOW, Category::Error, "DATETIME_FIELD_OVERFLOW", "datetime_field_overflow"),
    (SqlState::DATETIME_VALUE_OUT_OF_RANGE, Category::Error, "DATETIME_VALUE_OUT_OF_RANGE", ""),
    (SqlState::DIVISION_BY_ZERO, Category::Error, "DIVISION_BY_ZERO", "division_by_zero"),
    (SqlState::ERROR_IN_ASSIGNMENT, Category::Error, "ERROR_IN_ASSIGNMENT", "error_in_assignment"),
    (SqlState::ESCAPE_CHARACTER_CONFLICT, Category::Error, "ESCAPE_CHARACTER_CONFLICT", "escape_character_conflict"),
    (SqlState::INDICATOR_OVERFLOW, Category::Error, "INDICATOR_OVERFLOW", "indicator_overflow"),
    (SqlState::INTERVAL_FIELD_OVERFLOW, Category::Error, "INTERVAL_FIELD_OVERFLOW", "interval_field_overflow"),
    (SqlState::INVALID_ARGUMENT_FOR_LOG, Category::Error, "INVALID_ARGUMENT_FOR_LOG", "invalid_argument_for_logarithm"),
    (SqlState::INVALID_ARGUMENT_FOR_NTILE, Category::Error, "INVALID_ARGUMENT_FOR_NTILE", "invalid_argument_for_ntile_function"),
    (SqlState::INVALID_ARGUMENT_FOR_NTH_VALUE, Category::Error, "INVALID_ARGUMENT_FOR_NTH_VALUE", "invalid_argument_for_nth_value_function"),
    (SqlState::INVALID_ARGUMENT_FOR_POWER_FUNCTION, Category::Error, "INVALID_ARGUMENT_FOR_POWER_FUNCTION", "invalid_argument_for_power_function"),
    (SqlState::INVALID_ARGUMENT_FOR_WIDTH_BUCKET_FUNCTION, Category::Error, "INVALID_ARGUMENT_FOR_WIDTH_BUCKET_FUNCTION", "invalid_argument_for_width_bucket_function"),
    (SqlState::INVALID_CHARACTER_VALUE_FOR_CAST, Category::Error, "INVALID_CHARACTER_VALUE_FOR_CAST", "invalid_character_value_for_cast"),
    (SqlState::INVALID_DATETIME_FORMAT, Category::Error, "INVALID_DATETIME_FORMAT", "invalid_datetime_format"),
    (SqlState::INVALID_ESCAPE_CHARACTER, Category::Error, "INVALID_ESCAPE_CHARACTER", "invalid_escape_character"),
    (SqlState::INVALID_ESCAPE_OCTET, Category::Error, "INVALID_ESCAPE_OCTET", "invalid_escape_octet"),
    (SqlState::INVALID_ESCAPE_SEQUENCE, Category::Error, "INVALID_ESCAPE_SEQUENCE", "invalid_escape_sequence"),
    (SqlState::NONSTANDARD_USE_OF_ESCAPE_CHARACTER, Category::Error, "NONSTANDARD_USE_OF_ESCAPE_CHARACTER", "nonstandard_use_of_escape_character"),
    (SqlState::INVALID_INDICATOR_PARAMETER_VALUE, Category::Error, "INVALID_INDICATOR_PARAMETER_VALUE", "invalid_indicator_parameter_value"),
    (SqlState::INVALID_PARAMETER_VALUE, Category::Error, "INVALID_PARAMETER_VALUE", "invalid_parameter_value"),
    (SqlState::INVALID_PRECEDING_OR_FOLLOWING_SIZE, Category::Error, "INVALID_PRECEDING_OR_FOLLOWING_SIZE", "invalid_preceding_or_following_size"),
    (SqlState::INVALID_REGULAR_EXPRESSION, Category::Error, "INVALID_REGULAR_EXPRESSION", "invalid_regular_expression"),
    (SqlState::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE, Category::Error, "INVALID_ROW_COUNT_IN_LIMIT_CLAUSE", "invalid_row_count_in_limit_clause"),
    (SqlState::INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE, Category::Error, "INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE", "invalid_row_count_in_result_offset_clause"),
    (SqlState::INVALID_TABLESAMPLE_ARGUMENT, Category::Error, "INVALID_TABLESAMPLE_ARGUMENT", "invalid_tablesample_argument"),
    (SqlState::INVALID_TABLESAMPLE_REPEAT, Category::Error, "INVALID_TABLESAMPLE_REPEAT", "invalid_tablesample_repeat"),
    (SqlState::INVALID_TIME_ZONE_DISPLACEMENT_VALUE, Category::Error, "INVALID_TIME_ZONE_DISPLACEMENT_VALUE", "invalid_time_zone_displacement_value"),
    (SqlState::INVALID_USE_OF_ESCAPE_CHARACTER, Category::Error, "INVALID_USE_OF_ESCAPE_CHARACTER", "invalid_use_of_escape_character"),
    (SqlState::MOST_SPECIFIC_TYPE_MISMATCH, Category::Error, "MOST_SPECIFIC_TYPE_MISMATCH", "most_specific_type_mismatch"),
    (SqlState::NULL_VALUE_NOT_ALLOWED, Category::Error, "NULL_VALUE_NOT_ALLOWED", "null_value_not_allowed"),
    (SqlState::NULL_VALUE_NO_INDICATOR_PARAMETER, Category::Error, "NULL_VALUE_NO_INDICATOR_PARAMETER", "null_value_no_indicator_parameter"),
    (SqlState::NUMERIC_VALUE_OUT_OF_RANGE, Category::Error, "NUMERIC_VALUE_OUT_OF_RANGE", "numeric_value_out_of_range"),
    (SqlState::SEQUENCE_GENERATOR_LIMIT_EXCEEDED, Category::Error, "SEQUENCE_GENERATOR_LIMIT_EXCEEDED", "sequence_generator_limit_exceeded"),
    (SqlState::STRING_DATA_LENGTH_MISMATCH, Category::Error, "STRING_DATA_LENGTH_MISMATCH", "string_data_length_mismatch"),
    (SqlState::STRING_DATA_RIGHT_TRUNCATION, Category::Error, "STRING_DATA_RIGHT_TRUNCATION", "string_data_right_truncation"),
    (SqlState::SUBSTRING_ERROR, Category::Error, "SUBSTRING_ERROR", "substring_error"),
    (SqlState::TRIM_ERROR, Category::Error, "TRIM_ERROR", "trim_error"),
    (SqlState::UNTERMINATED_C_STRING, Category::Error, "UNTERMINATED_C_STRING", "unterminated_c_string"),
    (SqlState::ZERO_LENGTH_CHARACTER_STRING, Category::Error, "ZERO_LENGTH_CHARACTER_STRING", "zero_length_character_string"),
    (SqlState::FLOATING_POINT_EXCEPTION, Category::Error, "FLOATING_POINT_EXCEPTION", "floating_point_exception"),
    (SqlState::INVALID_TEXT_REPRESENTATION, Category::Error, "INVALID_TEXT_REPRESENTATION", "invalid_text_representation"),
    (SqlState::INVALID_BINARY_REPRESENTATION, Category::Error, "INVALID_BINARY_REPRESENTATION", "invalid_binary_representation"),
    (SqlState::BAD_COPY_FILE_FORMAT, Category::Error, "BAD_COPY_FILE_FORMAT", "bad_copy_file_format"),
    (SqlState::UNTRANSLATABLE_CHARACTER, Category::Error, "UNTRANSLATABLE_CHARACTER", "untranslatable_character"),
    (SqlState::NOT_AN_XML_DOCUMENT, Category::Error, "NOT_AN_XML_DOCUMENT", "not_an_xml_document"),
    (SqlState::INVALID_XML_DOCUMENT, Category::Error, "INVALID_XML_DOCUMENT", "invalid_xml_document"),
    (SqlState::INVALID_XML_CONTENT, Category::Error, "INVALID_XML_CONTENT", "invalid_xml_content"),
    (SqlState::INVALID_XML_COMMENT, Category::Error, "INVALID_XML_COMMENT", "invalid_xml_comment"),
    (SqlState::INVALID_XML_PROCESSING_INSTRUCTION, Category::Error, "INVALID_XML_PROCESSING_INSTRUCTION", "invalid_xml_processing_instruction"),
    (SqlState::DUPLICATE_JSON_OBJECT_KEY_VALUE, Category::Error, "DUPLICATE_JSON_OBJECT_KEY_VALUE", "duplicate_json_object_key_value"),
    (SqlState::INVALID_ARGUMENT_FOR_SQL_JSON_DATETIME_FUNCTION, Category::Error, "INVALID_ARGUMENT_FOR_SQL_JSON_DATETIME_FUNCTION", "invalid_argument_for_sql_json_datetime_function"),
    (SqlState::INVALID_JSON_TEXT, Category::Error, "INVALID_JSON_TEXT", "invalid_json_text"),
    (SqlState::INVALID_SQL_JSON_SUBSCRIPT, Category::Error, "INVALID_SQL_JSON_SUBSCRIPT", "invalid_sql_json_subscript"),
    (SqlState::MORE_THAN_ONE_SQL_JSON_ITEM, Category::Error, "MORE_THAN_ONE_SQL_JSON_ITEM", "more_than_one_sql_json_item"),
    (SqlState::NO_SQL_JSON_ITEM, Category::Error, "NO_SQL_JSON_ITEM", "no_sql_json_item"),
    (SqlState::NON_NUMERIC_SQL_JSON_ITEM, Category::Error, "NON_NUMERIC_SQL_JSON_ITEM", "non_numeric_sql_json_item"),
    (SqlState::NON_UNIQUE_KEYS_IN_A_JSON_OBJECT, Category::Error, "NON_UNIQUE_KEYS_IN_A_JSON_OBJECT", "non_unique_keys_in_a_json_object"),
    (SqlState::SINGLETON_SQL_JSON_ITEM_REQUIRED, Category::Error, "SINGLETON_SQL_JSON_ITEM_REQUIRED", "singleton_sql_json_item_required"),
    (SqlState::SQL_JSON_ARRAY_NOT_FOUND, Category::Error, "SQL_JSON_ARRAY_NOT_FOUND", "sql_json_array_not_found"),
    (SqlState::SQL_JSON_MEMBER_NOT_FOUND, Category::Error, "SQL_JSON_MEMBER_NOT_FOUND", "sql_json_member_not_found"),
    (SqlState::SQL_JSON_NUMBER_NOT_FOUND, Category::Error, "SQL_JSON_NUMBER_NOT_FOUND", "sql_json_number_not_found"),
    (SqlState::SQL_JSON_OBJECT_NOT_FOUND, Category::Error, "SQL_JSON_OBJECT_NOT_FOUND", "sql_json_object_not_found"),
    (SqlState::TOO_MANY_JSON_ARRAY_ELEMENTS, Category::Error, "TOO_MANY_JSON_ARRAY_ELEMENTS", "too_many_json_array_elements"),
    (SqlState::TOO_MANY_JSON_OBJECT_MEMBERS, Category::Error, "TOO_MANY_JSON_OBJECT_MEMBERS", "too_many_json_object_members"),
    (SqlState::SQL_JSON_SCALAR_REQUIRED, Category::Error, "SQL_JSON_SCALAR_REQUIRED", "sql_json_scalar_required"),
    (SqlState::SQL_JSON_ITEM_CANNOT_BE_CAST_TO_TARGET_TYPE, Category::Error, "SQL_JSON_ITEM_CANNOT_BE_CAST_TO_TARGET_TYPE", "sql_json_item_cannot_be_cast_to_target_type"),
    (SqlState::INTEGRITY_CONSTRAINT_VIOLATION, Category::Error, "INTEGRITY_CONSTRAINT_VIOLATION", "integrity_constraint_violation"),
    (SqlState::RESTRICT_VIOLATION, Category::Error, "RESTRICT_VIOLATION", "restrict_violation"),
    (SqlState::NOT_NULL_VIOLATION, Category::Error, "NOT_NULL_VIOLATION", "not_null_violation"),
    (SqlState::FOREIGN_KEY_VIOLATION, Category::Error, "FOREIGN_KEY_VIOLATION", "foreign_key_violation"),
    (SqlState::UNIQUE_VIOLATION, Category::Error, "UNIQUE_VIOLATION", "unique_violation"),
    (SqlState::CHECK_VIOLATION, Category::Error, "CHECK_VIOLATION", "check_violation"),
    (SqlState::EXCLUSION_VIOLATION, Category::Error, "EXCLUSION_VIOLATION", "exclusion_violation"),
    (SqlState::INVALID_CURSOR_STATE, Category::Error, "INVALID_CURSOR_STATE", "invalid_cursor_state"),
    (SqlState::INVALID_TRANSACTION_STATE, Category::Error, "INVALID_TRANSACTION_STATE", "invalid_transaction_state"),
    (SqlState::ACTIVE_SQL_TRANSACTION, Category::Error, "ACTIVE_SQL_TRANSACTION", "active_sql_transaction"),
    (SqlState::BRANCH_TRANSACTION_ALREADY_ACTIVE, Category::Error, "BRANCH_TRANSACTION_ALREADY_ACTIVE", "branch_transaction_already_active"),
    (SqlState::HELD_CURSOR_REQUIRES_SAME_ISOLATION_LEVEL, Category::Error, "HELD_CURSOR_REQUIRES_SAME_ISOLATION_LEVEL", "held_cursor_requires_same_isolation_level"),
    (SqlState::INAPPROPRIATE_ACCESS_MODE_FOR_BRANCH_TRANSACTION, Category::Error, "INAPPROPRIATE_ACCESS_MODE_FOR_BRANCH_TRANSACTION", "inappropriate_access_mode_for_branch_transaction"),
    (SqlState::INAPPROPRIATE_ISOLATION_LEVEL_FOR_BRANCH_TRANSACTION, Category::Error, "INAPPROPRIATE_ISOLATION_LEVEL_FOR_BRANCH_TRANSACTION", "inappropriate_isolation_level_for_branch_transaction"),
    (SqlState::NO_ACTIVE_SQL_TRANSACTION_FOR_BRANCH_TRANSACTION, Category::Error, "NO_ACTIVE_SQL_TRANSACTION_FOR_BRANCH_TRANSACTION", "no_active_sql_transaction_for_branch_transaction"),
    (SqlState::READ_ONLY_SQL_TRANSACTION, Category::Error, "READ_ONLY_SQL_TRANSACTION", "read_only_sql_transaction"),
    (SqlState::SCHEMA_AND_DATA_STATEMENT_MIXING_NOT_SUPPORTED, Category::Error, "SCHEMA_AND_DATA_STATEMENT_MIXING_NOT_SUPPORTED", "schema_and_data_statement_mixing_not_supported"),
    (SqlState::NO_ACTIVE_SQL_TRANSACTION, Category::Error, "NO_ACTIVE_SQL_TRANSACTION", "no_active_sql_transaction"),
    (SqlState::IN_FAILED_SQL_TRANSACTION, Category::Error, "IN_FAILED_SQL_TRANSACTION", "in_failed_sql_transaction"),
    (SqlState::IDLE_IN_TRANSACTION_SESSION_TIMEOUT, Category::Error, "IDLE_IN_TRANSACTION_SESSION_TIMEOUT", "idle_in_transaction_session_timeout"),
    (SqlState::TRANSACTION_TIMEOUT, Category::Error, "TRANSACTION_TIMEOUT", "transaction_timeout"),
    (SqlState::INVALID_SQL_STATEMENT_NAME, Category::Error, "INVALID_SQL_STATEMENT_NAME", "invalid_sql_statement_name"),
    (SqlState::TRIGGERED_DATA_CHANGE_VIOLATION, Category::Error, "TRIGGERED_DATA_CHANGE_VIOLATION", "triggered_data_change_violation"),
    (SqlState::INVALID_AUTHORIZATION_SPECIFICATION, Category::Error, "INVALID_AUTHORIZATION_SPECIFICATION", "invalid_authorization_specification"),
    (SqlState::INVALID_PASSWORD, Category::Error, "INVALID_PASSWORD", "invalid_password"),
    (SqlState::DEPENDENT_PRIVILEGE_DESCRIPTORS_STILL_EXIST, Category::Error, "DEPENDENT_PRIVILEGE_DESCRIPTORS_STILL_EXIST", "dependent_privilege_descriptors_still_exist"),
    (SqlState::DEPENDENT_OBJECTS_STILL_EXIST, Category::Error, "DEPENDENT_OBJECTS_STILL_EXIST", "dependent_objects_still_exist"),
    (SqlState::INVALID_TRANSACTION_TERMINATION, Category::Error, "INVALID_TRANSACTION_TERMINATION", "invalid_transaction_termination"),
    (SqlState::SQL_ROUTINE_EXCEPTION, Category::Error, "SQL_ROUTINE_EXCEPTION", "sql_routine_exception"),
    (SqlState::S_R_E_FUNCTION_EXECUTED_NO_RETURN_STATEMENT, Category::Error, "S_R_E_FUNCTION_EXECUTED_NO_RETURN_STATEMENT", "function_executed_no_return_statement"),
    (SqlState::S_R_E_MODIFYING_SQL_DATA_NOT_PERMITTED, Category::Error, "S_R_E_MODIFYING_SQL_DATA_NOT_PERMITTED", "modifying_sql_data_not_permitted"),
    (SqlState::S_R_E_PROHIBITED_SQL_STATEMENT_ATTEMPTED, Category::Error, "S_R_E_PROHIBITED_SQL_STATEMENT_ATTEMPTED", "prohibited_sql_statement_attempted"),
    (SqlState::S_R_E_READING_SQL_DATA_NOT_PERMITTED, Category::Error, "S_R_E_READING_SQL_DATA_NOT_PERMITTED", "reading_sql_data_not_permitted"),
    (SqlState::INVALID_CURSOR_NAME, Category::Error, "INVALID_CURSOR_NAME", "invalid_cursor_name"),
    (SqlState::EXTERNAL_ROUTINE_EXCEPTION, Category::Error, "EXTERNAL_ROUTINE_EXCEPTION", "external_routine_exception"),
    (SqlState::E_R_E_CONTAINING_SQL_NOT_PERMITTED, Category::Error, "E_R_E_CONTAINING_SQL_NOT_PERMITTED", "containing_sql_not_permitted"),
    (SqlState::E_R_E_MODIFYING_SQL_DATA_NOT_PERMITTED, Category::Error, "E_R_E_MODIFYING_SQL_DATA_NOT_PERMITTED", "modifying_sql_data_not_permitted"),
    (SqlState::E_R_E_PROHIBITED_SQL_STATEMENT_ATTEMPTED, Category::Error, "E_R_E_PROHIBITED_SQL_STATEMENT_ATTEMPTED", "prohibited_sql_statement_attempted"),
    (SqlState::E_R_E_READING_SQL_DATA_NOT_PERMITTED, Category::Error, "E_R_E_READING_SQL_DATA_NOT_PERMITTED", "reading_sql_data_not_permitted"),
    (SqlState::EXTERNAL_ROUTINE_INVOCATION_EXCEPTION, Category::Error, "EXTERNAL_ROUTINE_INVOCATION_EXCEPTION", "external_routine_invocation_exception"),
    (SqlState::E_R_I_E_INVALID_SQLSTATE_RETURNED, Category::Error, "E_R_I_E_INVALID_SQLSTATE_RETURNED", "invalid_sqlstate_returned"),
    (SqlState::E_R_I_E_NULL_VALUE_NOT_ALLOWED, Category::Error, "E_R_I_E_NULL_VALUE_NOT_ALLOWED", "null_value_not_allowed"),
    (SqlState::E_R_I_E_TRIGGER_PROTOCOL_VIOLATED, Category::Error, "E_R_I_E_TRIGGER_PROTOCOL_VIOLATED", "trigger_protocol_violated"),
    (SqlState::E_R_I_E_SRF_PROTOCOL_VIOLATED, Category::Error, "E_R_I_E_SRF_PROTOCOL_VIOLATED", "srf_protocol_violated"),
    (SqlState::E_R_I_E_EVENT_TRIGGER_PROTOCOL_VIOLATED, Category::Error, "E_R_I_E_EVENT_TRIGGER_PROTOCOL_VIOLATED", "event_trigger_protocol_violated"),
    (SqlState::SAVEPOINT_EXCEPTION, Category::Error, "SAVEPOINT_EXCEPTION", "savepoint_exception"),
    (SqlState::S_E_INVALID_SPECIFICATION, Category::Error, "S_E_INVALID_SPECIFICATION", "invalid_savepoint_specification"),
    (SqlState::INVALID_CATALOG_NAME, Category::Error, "INVALID_CATALOG_NAME", "invalid_catalog_name"),
    (SqlState::INVALID_SCHEMA_NAME, Category::Error, "INVALID_SCHEMA_NAME", "invalid_schema_name"),
    (SqlState::TRANSACTION_ROLLBACK, Category::Error, "TRANSACTION_ROLLBACK", "transaction_rollback"),
    (SqlState::T_R_INTEGRITY_CONSTRAINT_VIOLATION, Category::Error, "T_R_INTEGRITY_CONSTRAINT_VIOLATION", "transaction_integrity_constraint_violation"),
    (SqlState::T_R_SERIALIZATION_FAILURE, Category::Error, "T_R_SERIALIZATION_FAILURE", "serialization_failure"),
    (SqlState::T_R_STATEMENT_COMPLETION_UNKNOWN, Category::Error, "T_R_STATEMENT_COMPLETION_UNKNOWN", "statement_completion_unknown"),
    (SqlState::T_R_DEADLOCK_DETECTED, Category::Error, "T_R_DEADLOCK_DETECTED", "deadlock_detected"),
    (SqlState::SYNTAX_ERROR_OR_ACCESS_RULE_VIOLATION, Category::Error, "SYNTAX_ERROR_OR_ACCESS_RULE_VIOLATION", "syntax_error_or_access_rule_violation"),
    (SqlState::SYNTAX_ERROR, Category::Error, "SYNTAX_ERROR", "syntax_error"),
    (SqlState::INSUFFICIENT_PRIVILEGE, Category::Error, "INSUFFICIENT_PRIVILEGE", "insufficient_privilege"),
    (SqlState::CANNOT_COERCE, Category::Error, "CANNOT_COERCE", "cannot_coerce"),
    (SqlState::GROUPING_ERROR, Category::Error, "GROUPING_ERROR", "grouping_error"),
    (SqlState::WINDOWING_ERROR, Category::Error, "WINDOWING_ERROR", "windowing_error"),
    (SqlState::INVALID_RECURSION, Category::Error, "INVALID_RECURSION", "invalid_recursion"),
    (SqlState::INVALID_FOREIGN_KEY, Category::Error, "INVALID_FOREIGN_KEY", "invalid_foreign_key"),
    (SqlState::INVALID_NAME, Category::Error, "INVALID_NAME", "invalid_name"),
    (SqlState::NAME_TOO_LONG, Category::Error, "NAME_TOO_LONG", "name_too_long"),
    (SqlState::RESERVED_NAME, Category::Error, "RESERVED_NAME", "reserved_name"),
    (SqlState::DATATYPE_MISMATCH, Category::Error, "DATATYPE_MISMATCH", "datatype_mismatch"),
    (SqlState::INDETERMINATE_DATATYPE, Category::Error, "INDETERMINATE_DATATYPE", "indeterminate_datatype"),
    (SqlState::COLLATION_MISMATCH, Category::Error, "COLLATION_MISMATCH", "collation_mismatch"),
    (SqlState::INDETERMINATE_COLLATION, Category::Error, "INDETERMINATE_COLLATION", "indeterminate_collation"),
    (SqlState::WRONG_OBJECT_TYPE, Category::Error, "WRONG_OBJECT_TYPE", "wrong_object_type"),
    (SqlState::GENERATED_ALWAYS, Category::Error, "GENERATED_ALWAYS", "generated_always"),
    (SqlState::UNDEFINED_COLUMN, Category::Error, "UNDEFINED_COLUMN", "undefined_column"),
    (SqlState::UNDEFINED_CURSOR, Category::Error, "UNDEFINED_CURSOR", ""),
    (SqlState::UNDEFINED_DATABASE, Category::Error, "UNDEFINED_DATABASE", ""),
    (SqlState::UNDEFINED_FUNCTION, Category::Error, "UNDEFINED_FUNCTION", "undefined_function"),
    (SqlState::UNDEFINED_PSTATEMENT, Category::Error, "UNDEFINED_PSTATEMENT", ""),
    (SqlState::UNDEFINED_SCHEMA, Category::Error, "UNDEFINED_SCHEMA", ""),
    (SqlState::UNDEFINED_TABLE, Category::Error, "UNDEFINED_TABLE", "undefined_table"),
    (SqlState::UNDEFINED_PARAMETER, Category::Error, "UNDEFINED_PARAMETER", "undefined_parameter"),
    (SqlState::UNDEFINED_OBJECT, Category::Error, "UNDEFINED_OBJECT", "undefined_object"),
    (SqlState::DUPLICATE_COLUMN, Category::Error, "DUPLICATE_COLUMN", "duplicate_column"),
    (SqlState::DUPLICATE_CURSOR, Category::Error, "DUPLICATE_CURSOR", "duplicate_cursor"),
    (SqlState::DUPLICATE_DATABASE, Category::Error, "DUPLICATE_DATABASE", "duplicate_database"),
    (SqlState::DUPLICATE_FUNCTION, Category::Error, "DUPLICATE_FUNCTION", "duplicate_function"),
    (SqlState::DUPLICATE_PSTATEMENT, Category::Error, "DUPLICATE_PSTATEMENT", "duplicate_prepared_statement"),
    (SqlState::DUPLICATE_SCHEMA, Category::Error, "DUPLICATE_SCHEMA", "duplicate_schema"),
    (SqlState::DUPLICATE_TABLE, Category::Error, "DUPLICATE_TABLE", "duplicate_table"),
    (SqlState::DUPLICATE_ALIAS, Category::Error, "DUPLICATE_ALIAS", "duplicate_alias"),
    (SqlState::DUPLICATE_OBJECT, Category::Error, "DUPLICATE_OBJECT", "duplicate_object"),
    (SqlState::AMBIGUOUS_COLUMN, Category::Error, "AMBIGUOUS_COLUMN", "ambiguous_column"),
    (SqlState::AMBIGUOUS_FUNCTION, Category::Error, "AMBIGUOUS_FUNCTION", "ambiguous_function"),
    (SqlState::AMBIGUOUS_PARAMETER, Category::Error, "AMBIGUOUS_PARAMETER", "ambiguous_parameter"),
    (SqlState::AMBIGUOUS_ALIAS, Category::Error, "AMBIGUOUS_ALIAS", "ambiguous_alias"),
    (SqlState::INVALID_COLUMN_REFERENCE, Category::Error, "INVALID_COLUMN_REFERENCE", "invalid_column_reference"),
    (SqlState::INVALID_COLUMN_DEFINITION, Category::Error, "INVALID_COLUMN_DEFINITION", "invalid_column_definition"),
    (SqlState::INVALID_CURSOR_DEFINITION, Category::Error, "INVALID_CURSOR_DEFINITION", "invalid_cursor_definition"),
    (SqlState::INVALID_DATABASE_DEFINITION, Category::Error, "INVALID_DATABASE_DEFINITION", "invalid_database_definition"),
    (SqlState::INVALID_FUNCTION_DEFINITION, Category::Error, "INVALID_FUNCTION_DEFINITION", "invalid_function_definition"),
    (SqlState::INVALID_PSTATEMENT_DEFINITION, Category::Error, "INVALID_PSTATEMENT_DEFINITION", "invalid_prepared_statement_definition"),
    (SqlState::INVALID_SCHEMA_DEFINITION, Category::Error, "INVALID_SCHEMA_DEFINITION", "invalid_schema_definition"),
    (SqlState::INVALID_TABLE_DEFINITION, Category::Error, "INVALID_TABLE_DEFINITION", "invalid_table_definition"),
    (SqlState::INVALID_OBJECT_DEFINITION, Category::Error, "INVALID_OBJECT_DEFINITION", "invalid_object_definition"),
    (SqlState::WITH_CHECK_OPTION_VIOLATION, Category::Error, "WITH_CHECK_OPTION_VIOLATION", "with_check_option_violation"),
    (SqlState::INSUFFICIENT_RESOURCES, Category::Error, "INSUFFICIENT_RESOURCES", "insufficient_resources"),
    (SqlState::DISK_FULL, Category::Error, "DISK_FULL", "disk_full"),
    (SqlState::OUT_OF_MEMORY, Category::Error, "OUT_OF_MEMORY", "out_of_memory"),
    (SqlState::TOO_MANY_CONNECTIONS, Category::Error, "TOO_MANY_CONNECTIONS", "too_many_connections"),
    (SqlState::CONFIGURATION_LIMIT_EXCEEDED, Category::Error, "CONFIGURATION_LIMIT_EXCEEDED", "configuration_limit_exceeded"),
    (SqlState::PROGRAM_LIMIT_EXCEEDED, Category::Error, "PROGRAM_LIMIT_EXCEEDED", "program_limit_exceeded"),
    (SqlState::STATEMENT_TOO_COMPLEX, Category::Error, "STATEMENT_TOO_COMPLEX", "statement_too_complex"),
    (SqlState::TOO_MANY_COLUMNS, Category::Error, "TOO_MANY_COLUMNS", "too_many_columns"),
    (SqlState::TOO_MANY_ARGUMENTS, Category::Error, "TOO_MANY_ARGUMENTS", "too_many_arguments"),
    (SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, Category::Error, "OBJECT_NOT_IN_PREREQUISITE_STATE", "object_not_in_prerequisite_state"),
    (SqlState::OBJECT_IN_USE, Category::Error, "OBJECT_IN_USE", "object_in_use"),
    (SqlState::CANT_CHANGE_RUNTIME_PARAM, Category::Error, "CANT_CHANGE_RUNTIME_PARAM", "cant_change_runtime_param"),
    (SqlState::LOCK_NOT_AVAILABLE, Category::Error, "LOCK_NOT_AVAILABLE", "lock_not_available"),
    (SqlState::UNSAFE_NEW_ENUM_VALUE_USAGE, Category::Error, "UNSAFE_NEW_ENUM_VALUE_USAGE", "unsafe_new_enum_value_usage"),
    (SqlState::OPERATOR_INTERVENTION, Category::Error, "OPERATOR_INTERVENTION", "operator_intervention"),
    (SqlState::QUERY_CANCELED, Category::Error, "QUERY_CANCELED", "query_canceled"),
    (SqlState::ADMIN_SHUTDOWN, Category::Error, "ADMIN_SHUTDOWN", "admin_shutdown"),
    (SqlState::CRASH_SHUTDOWN, Category::Error, "CRASH_SHUTDOWN", "crash_shutdown"),
    (SqlState::CANNOT_CONNECT_NOW, Category::Error, "CANNOT_CONNECT_NOW", "cannot_connect_now"),
    (SqlState::DATABASE_DROPPED, Category::Error, "DATABASE_DROPPED", "database_dropped"),
    (SqlState::IDLE_SESSION_TIMEOUT, Category::Error, "IDLE_SESSION_TIMEOUT", "idle_session_timeout"),
    (SqlState::SYSTEM_ERROR, Category::Error, "SYSTEM_ERROR", "system_error"),
    (SqlState::IO_ERROR, Category::Error, "IO_ERROR", "io_error"),
    (SqlState::UNDEFINED_FILE, Category::Error, "UNDEFINED_FILE", "undefined_file"),
    (SqlState::DUPLICATE_FILE, Category::Error, "DUPLICATE_FILE", "duplicate_file"),
    (SqlState::FILE_NAME_TOO_LONG, Category::Error, "FILE_NAME_TOO_LONG", "file_name_too_long"),
    (SqlState::CONFIG_FILE_ERROR, Category::Error, "CONFIG_FILE_ERROR", "config_file_error"),
    (SqlState::LOCK_FILE_EXISTS, Category::Error, "LOCK_FILE_EXISTS", "lock_file_exists"),
    (SqlState::FDW_ERROR, Category::Error, "FDW_ERROR", "fdw_error"),
    (SqlState::FDW_COLUMN_NAME_NOT_FOUND, Category::Error, "FDW_COLUMN_NAME_NOT_FOUND", "fdw_column_name_not_found"),
    (SqlState::FDW_DYNAMIC_PARAMETER_VALUE_NEEDED, Category::Error, "FDW_DYNAMIC_PARAMETER_VALUE_NEEDED", "fdw_dynamic_parameter_value_needed"),
    (SqlState::FDW_FUNCTION_SEQUENCE_ERROR, Category::Error, "FDW_FUNCTION_SEQUENCE_ERROR", "fdw_function_sequence_error"),
    (SqlState::FDW_INCONSISTENT_DESCRIPTOR_INFORMATION, Category::Error, "FDW_INCONSISTENT_DESCRIPTOR_INFORMATION", "fdw_inconsistent_descriptor_information"),
    (SqlState::FDW_INVALID_ATTRIBUTE_VALUE, Category::Error, "FDW_INVALID_ATTRIBUTE_VALUE", "fdw_invalid_attribute_value"),
    (SqlState::FDW_INVALID_COLUMN_NAME, Category::Error, "FDW_INVALID_COLUMN_NAME", "fdw_invalid_column_name"),
    (SqlState::FDW_INVALID_COLUMN_NUMBER, Category::Error, "FDW_INVALID_COLUMN_NUMBER", "fdw_invalid_column_number"),
    (SqlState::FDW_INVALID_DATA_TYPE, Category::Error, "FDW_INVALID_DATA_TYPE", "fdw_invalid_data_type"),
    (SqlState::FDW_INVALID_DATA_TYPE_DESCRIPTORS, Category::Error, "FDW_INVALID_DATA_TYPE_DESCRIPTORS", "fdw_invalid_data_type_descriptors"),
    (SqlState::FDW_INVALID_DESCRIPTOR_FIELD_IDENTIFIER, Category::Error, "FDW_INVALID_DESCRIPTOR_FIELD_IDENTIFIER", "fdw_invalid_descriptor_field_identifier"),
    (SqlState::FDW_INVALID_HANDLE, Category::Error, "FDW_INVALID_HANDLE", "fdw_invalid_handle"),
    (SqlState::FDW_INVALID_OPTION_INDEX, Category::Error, "FDW_INVALID_OPTION_INDEX", "fdw_invalid_option_index"),
    (SqlState::FDW_INVALID_OPTION_NAME, Category::Error, "FDW_INVALID_OPTION_NAME", "fdw_invalid_option_name"),
    (SqlState::FDW_INVALID_STRING_LENGTH_OR_BUFFER_LENGTH, Category::Error, "FDW_INVALID_STRING_LENGTH_OR_BUFFER_LENGTH", "fdw_invalid_string_length_or_buffer_length"),
    (SqlState::FDW_INVALID_STRING_FORMAT, Category::Error, "FDW_INVALID_STRING_FORMAT", "fdw_invalid_string_format"),
    (SqlState::FDW_INVALID_USE_OF_NULL_POINTER, Category::Error, "FDW_INVALID_USE_OF_NULL_POINTER", "fdw_invalid_use_of_null_pointer"),
    (SqlState::FDW_TOO_MANY_HANDLES, Category::Error, "FDW_TOO_MANY_HANDLES", "fdw_too_many_handles"),
    (SqlState::FDW_OUT_OF_MEMORY, Category::Error, "FDW_OUT_OF_MEMORY", "fdw_out_of_memory"),
    (SqlState::FDW_NO_SCHEMAS, Category::Error, "FDW_NO_SCHEMAS", "fdw_no_schemas"),
    (SqlState::FDW_OPTION_NAME_NOT_FOUND, Category::Error, "FDW_OPTION_NAME_NOT_FOUND", "fdw_option_name_not_found"),
    (SqlState::FDW_REPLY_HANDLE, Category::Error, "FDW_REPLY_HANDLE", "fdw_reply_handle"),
    (SqlState::FDW_SCHEMA_NOT_FOUND, Category::Error, "FDW_SCHEMA_NOT_FOUND", "fdw_schema_not_found"),
    (SqlState::FDW_TABLE_NOT_FOUND, Category::Error, "FDW_TABLE_NOT_FOUND", "fdw_table_not_found"),
    (SqlState::FDW_UNABLE_TO_CREATE_EXECUTION, Category::Error, "FDW_UNABLE_TO_CREATE_EXECUTION", "fdw_unable_to_create_execution"),
    (SqlState::FDW_UNABLE_TO_CREATE_REPLY, Category::Error, "FDW_UNABLE_TO_CREATE_REPLY", "fdw_unable_to_create_reply"),
    (SqlState::FDW_UNABLE_TO_ESTABLISH_CONNECTION, Category::Error, "FDW_UNABLE_TO_ESTABLISH_CONNECTION", "fdw_unable_to_establish_connection"),
    (SqlState::PLPGSQL_ERROR, Category::Error, "PLPGSQL_ERROR", "plpgsql_error"),
    (SqlState::RAISE_EXCEPTION, Category::Error, "RAISE_EXCEPTION", "raise_exception"),
    (SqlState::NO_DATA_FOUND, Category::Error, "NO_DATA_FOUND", "no_data_found"),
    (SqlState::TOO_MANY_ROWS, Category::Error, "TOO_MANY_ROWS", "too_many_rows"),
    (SqlState::ASSERT_FAILURE, Category::Error, "ASSERT_FAILURE", "assert_failure"),
    (SqlState::INTERNAL_ERROR, Category::Error, "INTERNAL_ERROR", "internal_error"),
    (SqlState::DATA_CORRUPTED, Category::Error, "DATA_CORRUPTED", "data_corrupted"),
    (SqlState::INDEX_CORRUPTED, Category::Error, "INDEX_CORRUPTED", "index_corrupted"),
];

/// Every class, in file order: the first two characters of its codes and its title.
pub(crate) static CLASSES: [(&str, &str); 43] = [
    ("00", "Successful Completion"),
    ("01", "Warning"),
    ("02", "No Data (this is also a warning class per the SQL standard)"),
    ("03", "SQL Statement Not Yet Complete"),
    ("08", "Connection Exception"),
    ("09", "Triggered Action Exception"),
    ("0A", "Feature Not Supported"),
    ("0B", "Invalid Transaction Initiation"),
    ("0F", "Locator Exception"),
    ("0L", "Invalid Grantor"),
    ("0P", "Invalid Role Specification"),
    ("0Z", "Diagnostics Exception"),
    ("10", "XQuery Error"),
    ("20", "Case Not Found"),
    ("21", "Cardinality Violation"),
    ("22", "Data Exception"),
    ("23", "Integrity Constraint Violation"),
    ("24", "Invalid Cursor State"),
    ("25", "Invalid Transaction State"),
    ("26", "Invalid SQL Statement Name"),
    ("27", "Triggered Data Change Violation"),
    ("28", "Invalid Authorization Specification"),
    ("2B", "Dependent Privilege Descriptors Still Exist"),
    ("2D", "Invalid Transaction Termination"),
    ("2F", "SQL Routine Exception"),
    ("34", "Invalid Cursor Name"),
    ("38", "External Routine Exception"),
    ("39", "External Routine Invocation Exception"),
    ("3B", "Savepoint Exception"),
    ("3D", "Invalid Catalog Name"),
    ("3F", "Invalid Schema Name"),
    ("40", "Transaction Rollback"),
    ("42", "Syntax Error or Access Rule Violation"),
    ("44", "WITH CHECK OPTION Violation"),
    ("53", "Insufficient Resources"),
    ("54", "Program Limit Exceeded"),
    ("55", "Object Not In Prerequisite State"),
    ("57", "Operator Intervention"),
    ("58", "System Error (errors external to PostgreSQL itself)"),
    ("F0", "Configuration File Error"),
    ("HV", "Foreign Data Wrapper Error (SQL/MED)"),
    ("P0", "PL/pgSQL Error"),
    ("XX", "Internal Error"),
];
