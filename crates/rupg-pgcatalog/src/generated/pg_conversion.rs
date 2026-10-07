//! The 98 static rows of `pg_conversion`, one batch for each column.
//!
//! `cargo xtask pgcatalog` makes this file from the catalog headers and the `.dat` files in `vendor/postgres-19/src/include/catalog`. Do not edit it.

use crate::{Batch, Values};

pub(crate) static COLUMNS: [Batch; 8] = [
    // oid
    Batch {
        values: Values::Oid(&[
            4410, 4411, 4412, 4413, 4414, 4415, 4416, 4417, 4418, 4419, 4420, 4421, 4424, 4425,
            4432, 4433, 4442, 4443, 4452, 4453, 4454, 4455, 4456, 4457, 4458, 4459, 4460, 4461,
            4462, 4463, 4464, 4465, 4466, 4467, 4468, 4469, 4470, 4471, 4472, 4473, 4474, 4475,
            4476, 4477, 4478, 4479, 4480, 4481, 4482, 4483, 4484, 4485, 4486, 4487, 4488, 4489,
            4490, 4491, 4492, 4493, 4494, 4495, 4496, 4497, 4498, 4499, 4500, 4501, 4502, 4503,
            4504, 4505, 4506, 4507, 4508, 4509, 4510, 4511, 4512, 4513, 4514, 4515, 4516, 4517,
            4518, 4519, 4520, 4521, 4522, 4523, 4524, 4525, 4526, 4527, 4528, 4529, 4530, 4531,
        ]),
        nulls: &[],
    },
    // conname
    Batch {
        values: Values::Text(&[
            "koi8_r_to_windows_1251", "windows_1251_to_koi8_r", "koi8_r_to_windows_866",
            "windows_866_to_koi8_r", "windows_866_to_windows_1251", "windows_1251_to_windows_866",
            "iso_8859_5_to_koi8_r", "koi8_r_to_iso_8859_5", "iso_8859_5_to_windows_1251",
            "windows_1251_to_iso_8859_5", "iso_8859_5_to_windows_866", "windows_866_to_iso_8859_5",
            "euc_jp_to_sjis", "sjis_to_euc_jp", "euc_tw_to_big5", "big5_to_euc_tw",
            "iso_8859_2_to_windows_1250", "windows_1250_to_iso_8859_2", "big5_to_utf8",
            "utf8_to_big5", "utf8_to_koi8_r", "koi8_r_to_utf8", "utf8_to_koi8_u", "koi8_u_to_utf8",
            "utf8_to_windows_866", "windows_866_to_utf8", "utf8_to_windows_874",
            "windows_874_to_utf8", "utf8_to_windows_1250", "windows_1250_to_utf8",
            "utf8_to_windows_1251", "windows_1251_to_utf8", "utf8_to_windows_1252",
            "windows_1252_to_utf8", "utf8_to_windows_1253", "windows_1253_to_utf8",
            "utf8_to_windows_1254", "windows_1254_to_utf8", "utf8_to_windows_1255",
            "windows_1255_to_utf8", "utf8_to_windows_1256", "windows_1256_to_utf8",
            "utf8_to_windows_1257", "windows_1257_to_utf8", "utf8_to_windows_1258",
            "windows_1258_to_utf8", "euc_cn_to_utf8", "utf8_to_euc_cn", "euc_jp_to_utf8",
            "utf8_to_euc_jp", "euc_kr_to_utf8", "utf8_to_euc_kr", "euc_tw_to_utf8",
            "utf8_to_euc_tw", "gb18030_to_utf8", "utf8_to_gb18030", "gbk_to_utf8", "utf8_to_gbk",
            "utf8_to_iso_8859_2", "iso_8859_2_to_utf8", "utf8_to_iso_8859_3", "iso_8859_3_to_utf8",
            "utf8_to_iso_8859_4", "iso_8859_4_to_utf8", "utf8_to_iso_8859_9", "iso_8859_9_to_utf8",
            "utf8_to_iso_8859_10", "iso_8859_10_to_utf8", "utf8_to_iso_8859_13",
            "iso_8859_13_to_utf8", "utf8_to_iso_8859_14", "iso_8859_14_to_utf8",
            "utf8_to_iso_8859_15", "iso_8859_15_to_utf8", "utf8_to_iso_8859_16",
            "iso_8859_16_to_utf8", "utf8_to_iso_8859_5", "iso_8859_5_to_utf8", "utf8_to_iso_8859_6",
            "iso_8859_6_to_utf8", "utf8_to_iso_8859_7", "iso_8859_7_to_utf8", "utf8_to_iso_8859_8",
            "iso_8859_8_to_utf8", "iso_8859_1_to_utf8", "utf8_to_iso_8859_1", "johab_to_utf8",
            "utf8_to_johab", "sjis_to_utf8", "utf8_to_sjis", "uhc_to_utf8", "utf8_to_uhc",
            "euc_jis_2004_to_utf8", "utf8_to_euc_jis_2004", "shift_jis_2004_to_utf8",
            "utf8_to_shift_jis_2004", "euc_jis_2004_to_shift_jis_2004",
            "shift_jis_2004_to_euc_jis_2004",
        ]),
        nulls: &[],
    },
    // connamespace
    Batch {
        values: Values::Oid(&[
            11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11,
            11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11,
            11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11,
            11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11,
            11, 11, 11, 11, 11, 11, 11, 11, 11, 11,
        ]),
        nulls: &[],
    },
    // conowner
    Batch {
        values: Values::Oid(&[
            10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
            10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
            10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
            10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
            10, 10, 10, 10, 10, 10, 10, 10, 10, 10,
        ]),
        nulls: &[],
    },
    // conforencoding
    Batch {
        values: Values::Int4(&[
            22, 23, 22, 20, 20, 23, 25, 22, 25, 23, 25, 20, 1, 35, 4, 36, 9, 29, 36, 6, 6, 22, 6,
            34, 6, 20, 6, 21, 6, 29, 6, 23, 6, 24, 6, 30, 6, 31, 6, 32, 6, 18, 6, 33, 6, 19, 2, 6,
            1, 6, 3, 6, 4, 6, 39, 6, 37, 6, 6, 9, 6, 10, 6, 11, 6, 12, 6, 13, 6, 14, 6, 15, 6, 16,
            6, 17, 6, 25, 6, 26, 6, 27, 6, 28, 8, 6, 40, 6, 35, 6, 38, 6, 5, 6, 41, 6, 5, 41,
        ]),
        nulls: &[],
    },
    // contoencoding
    Batch {
        values: Values::Int4(&[
            23, 22, 20, 22, 23, 20, 22, 25, 23, 25, 20, 25, 35, 1, 36, 4, 29, 9, 6, 36, 22, 6, 34,
            6, 20, 6, 21, 6, 29, 6, 23, 6, 24, 6, 30, 6, 31, 6, 32, 6, 18, 6, 33, 6, 19, 6, 6, 2, 6,
            1, 6, 3, 6, 4, 6, 39, 6, 37, 9, 6, 10, 6, 11, 6, 12, 6, 13, 6, 14, 6, 15, 6, 16, 6, 17,
            6, 25, 6, 26, 6, 27, 6, 28, 6, 6, 8, 6, 40, 6, 35, 6, 38, 6, 5, 6, 41, 41, 5,
        ]),
        nulls: &[],
    },
    // conproc
    Batch {
        values: Values::Oid(&[
            4310, 4311, 4312, 4313, 4314, 4315, 4316, 4317, 4318, 4319, 4320, 4321, 4324, 4325,
            4332, 4333, 4342, 4343, 4352, 4353, 4354, 4355, 4356, 4357, 4358, 4359, 4358, 4359,
            4358, 4359, 4358, 4359, 4358, 4359, 4358, 4359, 4358, 4359, 4358, 4359, 4358, 4359,
            4358, 4359, 4358, 4359, 4360, 4361, 4362, 4363, 4364, 4365, 4366, 4367, 4368, 4369,
            4370, 4371, 4372, 4373, 4372, 4373, 4372, 4373, 4372, 4373, 4372, 4373, 4372, 4373,
            4372, 4373, 4372, 4373, 4372, 4373, 4372, 4373, 4372, 4373, 4372, 4373, 4372, 4373,
            4374, 4375, 4376, 4377, 4378, 4379, 4380, 4381, 4382, 4383, 4384, 4385, 4386, 4387,
        ]),
        nulls: &[],
    },
    // condefault
    Batch {
        values: Values::Bool(&[
            true, true, true, true, true, true, true, true, true, true, true, true, true, true,
            true, true, true, true, true, true, true, true, true, true, true, true, true, true,
            true, true, true, true, true, true, true, true, true, true, true, true, true, true,
            true, true, true, true, true, true, true, true, true, true, true, true, true, true,
            true, true, true, true, true, true, true, true, true, true, true, true, true, true,
            true, true, true, true, true, true, true, true, true, true, true, true, true, true,
            true, true, true, true, true, true, true, true, true, true, true, true, true, true,
        ]),
        nulls: &[],
    },
];
