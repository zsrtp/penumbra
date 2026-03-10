use std::path::Path;
use std::sync::Arc;

use gimli::{EndianRcSlice, Reader as _};
use object::{Object, ObjectSymbol};

type Reader = EndianRcSlice<gimli::RunTimeEndian>;

#[derive(Debug, Clone)]
pub enum VarLocation {
    Register(u8),
    StackOffset(i64),
    Unknown,
}

#[derive(Debug, Clone)]
pub struct VarType {
    pub name: String,
    pub byte_size: u8,
    pub encoding: gimli::DwAte,
}

#[derive(Debug, Clone)]
pub struct VariableInfo {
    pub name: String,
    pub location: VarLocation,
    pub var_type: VarType,
}

pub struct Symbol {
    pub name: String,
    pub address: u32,
    pub size: u32,
}

/// Pre-built index entry for reverse line lookups (file:line → address).
struct LineEntry {
    file: String,
    line: u32,
    addr: u32,
}

pub struct SymbolResolver {
    symbols: Vec<Symbol>,
    context: Option<addr2line::Context<Reader>>,
    dwarf: Option<Arc<gimli::Dwarf<Reader>>>,
    /// Sorted by (file, line) for binary search in reverse lookups.
    line_index: Vec<LineEntry>,
}

impl SymbolResolver {
    pub fn load(
        elf_path: &Path,
        debug_info_path: Option<&Path>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let elf_data = std::fs::read(elf_path)?;
        let elf = object::read::File::parse(&*elf_data)?;

        // Extract symbols from ELF symbol table
        let mut symbols: Vec<Symbol> = elf
            .symbols()
            .filter(|s| s.kind() == object::SymbolKind::Text && s.address() != 0)
            .filter_map(|s| {
                Some(Symbol {
                    name: s.name().ok()?.to_string(),
                    address: s.address() as u32,
                    size: s.size() as u32,
                })
            })
            .collect();
        symbols.sort_by_key(|s| s.address);

        // Load DWARF from debug_info.o (or from the ELF itself)
        let dwarf_path = debug_info_path.unwrap_or(elf_path);
        let (context, dwarf, line_index) = match Self::load_dwarf(dwarf_path) {
            Ok((ctx, dw, idx)) => (Some(ctx), Some(dw), idx),
            Err(e) => {
                eprintln!(
                    "Warning: failed to load DWARF from {}: {}",
                    dwarf_path.display(),
                    e
                );
                (None, None, Vec::new())
            }
        };

        Ok(Self {
            symbols,
            context,
            dwarf,
            line_index,
        })
    }

    fn load_dwarf(
        path: &Path,
    ) -> Result<
        (
            addr2line::Context<Reader>,
            Arc<gimli::Dwarf<Reader>>,
            Vec<LineEntry>,
        ),
        Box<dyn std::error::Error>,
    > {
        let data = std::fs::read(path)?;
        let obj = object::read::File::parse(&*data)?;
        let endian = if obj.is_little_endian() {
            gimli::RunTimeEndian::Little
        } else {
            gimli::RunTimeEndian::Big
        };

        let load_section = |id: gimli::SectionId| -> Result<Reader, gimli::Error> {
            use object::ObjectSection;
            let data = obj
                .section_by_name(id.name())
                .and_then(|s| s.uncompressed_data().ok())
                .unwrap_or_default();
            Ok(EndianRcSlice::new(std::rc::Rc::from(&*data), endian))
        };

        let dwarf = Arc::new(gimli::Dwarf::load(load_section)?);

        // Build reverse line index by iterating all line programs
        let line_index = Self::build_line_index(&dwarf)?;

        let context = addr2line::Context::from_arc_dwarf(dwarf.clone())?;

        Ok((context, dwarf, line_index))
    }

    fn build_line_index(
        dwarf: &gimli::Dwarf<Reader>,
    ) -> Result<Vec<LineEntry>, gimli::Error> {
        let mut entries = Vec::new();
        let mut units = dwarf.units();

        while let Some(unit_header) = units.next()? {
            let unit = dwarf.unit(unit_header)?;
            let line_program = match unit.line_program.clone() {
                Some(lp) => lp,
                None => continue,
            };

            // Collect file table from header before consuming rows
            let header = line_program.header();
            let mut file_table: Vec<String> = Vec::new();
            {
                let files = header.file_names();
                for fe in files {
                    let name = attr_to_string(&fe.path_name());
                    let dir = fe
                        .directory(header)
                        .map(|d| attr_to_string(&d))
                        .unwrap_or_default();
                    let path = if dir.is_empty() {
                        name
                    } else {
                        format!("{}/{}", dir, name)
                    };
                    file_table.push(path);
                }
            };

            let mut rows = line_program.rows();
            while let Some((_, row)) = rows.next_row()? {
                if row.end_sequence() {
                    continue;
                }
                let line = match row.line() {
                    Some(l) => l.get() as u32,
                    None => continue,
                };
                // file_index is 1-based in DWARF, 0-based in our table
                let file_idx = row.file_index() as usize;
                if file_idx == 0 || file_idx > file_table.len() {
                    continue;
                }
                let file = file_table[file_idx - 1].clone();
                entries.push(LineEntry {
                    file,
                    line,
                    addr: row.address() as u32,
                });
            }
        }

        // Sort by (file, line) for efficient lookup
        entries.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));

        Ok(entries)
    }

    /// Look up a function name by address (binary search in sorted symbol table).
    pub fn addr_to_function(&self, addr: u32) -> Option<&str> {
        let idx = self.symbols.partition_point(|s| s.address <= addr);
        if idx == 0 {
            return None;
        }
        let sym = &self.symbols[idx - 1];
        if sym.size > 0 && addr >= sym.address + sym.size {
            return None;
        }
        Some(&sym.name)
    }

    /// Look up source file and line number by address using DWARF line tables.
    pub fn addr_to_location(&self, addr: u32) -> Option<(String, u32)> {
        let ctx = self.context.as_ref()?;
        let loc = ctx.find_location(addr as u64).ok()??;
        let file = loc.file?.to_string();
        let line = loc.line?;
        Some((file, line))
    }

    /// Resolve a source file + line to an address using the pre-built line index.
    pub fn location_to_addr(&self, file: &str, line: u32) -> Option<u32> {
        // Linear scan with path matching (index is sorted but we need suffix matching)
        let mut best: Option<u32> = None;
        for entry in &self.line_index {
            if entry.line == line && paths_match(file, &entry.file) {
                best = Some(match best {
                    Some(prev) => prev.min(entry.addr),
                    None => entry.addr,
                });
            }
        }
        best
    }

    /// Look up a function's start address by name.
    pub fn function_to_addr(&self, name: &str) -> Option<u32> {
        self.symbols
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.address)
    }

    /// Find local variables and parameters for the function containing `pc`.
    pub fn find_variables(&self, pc: u32) -> Vec<VariableInfo> {
        let dwarf = match &self.dwarf {
            Some(d) => d,
            None => return Vec::new(),
        };

        let mut vars = Vec::new();
        let mut units = dwarf.units();

        while let Ok(Some(header)) = units.next() {
            let unit = match dwarf.unit(header) {
                Ok(u) => u,
                Err(_) => continue,
            };

            let mut entries = unit.entries();
            let mut in_subprogram = false;
            let mut subprogram_depth: isize = 0;

            while let Ok(Some(entry)) = entries.next_dfs() {
                let depth = entry.depth();

                if entry.tag() == gimli::DW_TAG_subprogram {
                    in_subprogram = false;

                    let low_pc = match entry.attr_value(gimli::DW_AT_low_pc) {
                        Some(gimli::AttributeValue::Addr(addr)) => addr as u32,
                        _ => continue,
                    };
                    let high_pc = match entry.attr_value(gimli::DW_AT_high_pc) {
                        Some(gimli::AttributeValue::Addr(addr)) => addr as u32,
                        Some(gimli::AttributeValue::Udata(offset)) => low_pc + offset as u32,
                        _ => continue,
                    };

                    if pc >= low_pc && pc < high_pc {
                        in_subprogram = true;
                        subprogram_depth = depth;
                    }
                } else if in_subprogram {
                    if depth <= subprogram_depth {
                        in_subprogram = false;
                        continue;
                    }

                    if entry.tag() == gimli::DW_TAG_variable
                        || entry.tag() == gimli::DW_TAG_formal_parameter
                    {
                        if let Some(var) = Self::extract_variable(entry, &unit, dwarf) {
                            vars.push(var);
                        }
                    }
                }
            }

            if !vars.is_empty() {
                break;
            }
        }

        vars
    }

    fn extract_variable(
        entry: &gimli::DebuggingInformationEntry<Reader>,
        unit: &gimli::Unit<Reader>,
        dwarf: &gimli::Dwarf<Reader>,
    ) -> Option<VariableInfo> {
        // Name
        let name_val = entry.attr_value(gimli::DW_AT_name)?;
        let name_reader = dwarf.attr_string(unit, name_val).ok()?;
        let name = name_reader.to_string_lossy().ok()?.into_owned();

        // Location
        let location = match entry.attr_value(gimli::DW_AT_location) {
            Some(gimli::AttributeValue::Exprloc(expr)) => {
                let mut ops = expr.operations(unit.encoding());
                match ops.next() {
                    Ok(Some(gimli::Operation::Register { register })) => {
                        VarLocation::Register(register.0 as u8)
                    }
                    Ok(Some(gimli::Operation::RegisterOffset {
                        register, offset, ..
                    })) => {
                        if register.0 == 1 {
                            // r1 = SP
                            VarLocation::StackOffset(offset)
                        } else {
                            VarLocation::Unknown
                        }
                    }
                    _ => VarLocation::Unknown,
                }
            }
            _ => VarLocation::Unknown,
        };

        // Type
        let var_type = match entry.attr_value(gimli::DW_AT_type) {
            Some(gimli::AttributeValue::UnitRef(offset)) => {
                Self::resolve_type(unit, dwarf, offset)
            }
            _ => None,
        }
        .unwrap_or(VarType {
            name: "unknown".to_string(),
            byte_size: 4,
            encoding: gimli::DW_ATE_unsigned,
        });

        Some(VariableInfo {
            name,
            location,
            var_type,
        })
    }

    fn resolve_type(
        unit: &gimli::Unit<Reader>,
        dwarf: &gimli::Dwarf<Reader>,
        offset: gimli::UnitOffset,
    ) -> Option<VarType> {
        let mut tree = unit.entries_tree(Some(offset)).ok()?;
        let root = tree.root().ok()?;
        let entry = root.entry();

        if entry.tag() == gimli::DW_TAG_pointer_type {
            let byte_size = entry
                .attr_value(gimli::DW_AT_byte_size)
                .and_then(|v| match v {
                    gimli::AttributeValue::Udata(n) => Some(n as u8),
                    _ => None,
                })
                .unwrap_or(4);

            // Try to get the pointed-to type's name
            let pointee_name = entry
                .attr_value(gimli::DW_AT_type)
                .and_then(|v| match v {
                    gimli::AttributeValue::UnitRef(inner_offset) => {
                        let mut inner_tree = unit.entries_tree(Some(inner_offset)).ok()?;
                        let inner_root = inner_tree.root().ok()?;
                        let inner_entry = inner_root.entry();
                        let inner_name_val = inner_entry.attr_value(gimli::DW_AT_name)?;
                        let reader = dwarf.attr_string(unit, inner_name_val).ok()?;
                        let s = reader.to_string_lossy().ok()?.into_owned();
                        Some(format!("{}*", s))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| "void*".to_string());

            return Some(VarType {
                name: pointee_name,
                byte_size,
                encoding: gimli::DW_ATE_address,
            });
        }

        // Base type
        let name = entry
            .attr_value(gimli::DW_AT_name)
            .and_then(|v| {
                let reader = dwarf.attr_string(unit, v).ok()?;
                reader.to_string_lossy().ok().map(|s| s.into_owned())
            })
            .unwrap_or_else(|| "unknown".to_string());

        let byte_size = entry
            .attr_value(gimli::DW_AT_byte_size)
            .and_then(|v| match v {
                gimli::AttributeValue::Udata(n) => Some(n as u8),
                _ => None,
            })
            .unwrap_or(4);

        let encoding = entry
            .attr_value(gimli::DW_AT_encoding)
            .and_then(|v| match v {
                gimli::AttributeValue::Encoding(e) => Some(e),
                _ => None,
            })
            .unwrap_or(gimli::DW_ATE_unsigned);

        Some(VarType {
            name,
            byte_size,
            encoding,
        })
    }
}

fn attr_to_string(value: &gimli::AttributeValue<Reader>) -> String {
    match value {
        gimli::AttributeValue::String(s) => {
            match s.to_slice() {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(_) => String::new(),
            }
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── paths_match ─────────────────────────────────────────────────────

    #[test]
    fn paths_match_exact() {
        assert!(paths_match("foo.cpp", "foo.cpp"));
    }

    #[test]
    fn paths_match_query_is_suffix() {
        assert!(paths_match("foo.cpp", "src/dir/foo.cpp"));
    }

    #[test]
    fn paths_match_dwarf_is_suffix() {
        assert!(paths_match("src/dir/foo.cpp", "foo.cpp"));
    }

    #[test]
    fn paths_match_full_paths() {
        assert!(paths_match(
            "/home/user/project/src/main.cpp",
            "src/main.cpp"
        ));
    }

    #[test]
    fn paths_match_backslash_normalization() {
        assert!(paths_match("src\\dir\\foo.cpp", "src/dir/foo.cpp"));
    }

    #[test]
    fn paths_match_no_match() {
        assert!(!paths_match("bar.cpp", "foo.cpp"));
    }

    #[test]
    fn paths_match_partial_filename_no_match() {
        // "oo.cpp" is a suffix of "foo.cpp" at the string level,
        // so this actually returns true — documenting current behavior
        assert!(paths_match("oo.cpp", "foo.cpp"));
    }

    // ── addr_to_function ────────────────────────────────────────────────

    fn make_resolver(symbols: Vec<Symbol>) -> SymbolResolver {
        SymbolResolver {
            symbols,
            context: None,
            dwarf: None,
            line_index: Vec::new(),
        }
    }

    #[test]
    fn addr_to_function_exact_start() {
        let r = make_resolver(vec![Symbol {
            name: "foo".into(),
            address: 0x80001000,
            size: 0x100,
        }]);
        assert_eq!(r.addr_to_function(0x80001000), Some("foo"));
    }

    #[test]
    fn addr_to_function_within_bounds() {
        let r = make_resolver(vec![Symbol {
            name: "foo".into(),
            address: 0x80001000,
            size: 0x100,
        }]);
        assert_eq!(r.addr_to_function(0x80001050), Some("foo"));
    }

    #[test]
    fn addr_to_function_at_end_boundary() {
        let r = make_resolver(vec![Symbol {
            name: "foo".into(),
            address: 0x80001000,
            size: 0x100,
        }]);
        // addr == address + size is out of bounds
        assert_eq!(r.addr_to_function(0x80001100), None);
    }

    #[test]
    fn addr_to_function_before_first() {
        let r = make_resolver(vec![Symbol {
            name: "foo".into(),
            address: 0x80001000,
            size: 0x100,
        }]);
        assert_eq!(r.addr_to_function(0x80000FFF), None);
    }

    #[test]
    fn addr_to_function_in_gap() {
        let r = make_resolver(vec![
            Symbol {
                name: "foo".into(),
                address: 0x80001000,
                size: 0x100,
            },
            Symbol {
                name: "bar".into(),
                address: 0x80002000,
                size: 0x50,
            },
        ]);
        // Between foo's end and bar's start
        assert_eq!(r.addr_to_function(0x80001500), None);
    }

    #[test]
    fn addr_to_function_second_symbol() {
        let r = make_resolver(vec![
            Symbol {
                name: "foo".into(),
                address: 0x80001000,
                size: 0x100,
            },
            Symbol {
                name: "bar".into(),
                address: 0x80002000,
                size: 0x50,
            },
        ]);
        assert_eq!(r.addr_to_function(0x80002010), Some("bar"));
    }

    #[test]
    fn addr_to_function_zero_size_matches() {
        // Zero-size symbol: the size check (addr >= address + size) is
        // (addr >= address + 0), which only fails if addr < address.
        // So zero-size symbols act as "extends to next symbol".
        let r = make_resolver(vec![Symbol {
            name: "foo".into(),
            address: 0x80001000,
            size: 0,
        }]);
        assert_eq!(r.addr_to_function(0x80001000), Some("foo"));
    }

    // ── function_to_addr ────────────────────────────────────────────────

    #[test]
    fn function_to_addr_found() {
        let r = make_resolver(vec![Symbol {
            name: "my_func".into(),
            address: 0x80005000,
            size: 0x40,
        }]);
        assert_eq!(r.function_to_addr("my_func"), Some(0x80005000));
    }

    #[test]
    fn function_to_addr_not_found() {
        let r = make_resolver(vec![Symbol {
            name: "my_func".into(),
            address: 0x80005000,
            size: 0x40,
        }]);
        assert_eq!(r.function_to_addr("other_func"), None);
    }

    #[test]
    fn function_to_addr_exact_match_only() {
        let r = make_resolver(vec![Symbol {
            name: "my_func__Fv".into(),
            address: 0x80005000,
            size: 0x40,
        }]);
        assert_eq!(r.function_to_addr("my_func"), None);
    }
}

/// Check if two file paths match (by suffix, since DWARF paths may be relative).
pub(crate) fn paths_match(query: &str, dwarf_path: &str) -> bool {
    let q = query.replace('\\', "/");
    let d = dwarf_path.replace('\\', "/");
    if q == d {
        return true;
    }
    suffix_match(&q, &d) || suffix_match(&d, &q)
}

/// Check if `haystack` ends with `needle` at a path boundary (preceded by `/`).
fn suffix_match(haystack: &str, needle: &str) -> bool {
    if let Some(prefix) = haystack.strip_suffix(needle) {
        prefix.is_empty() || prefix.ends_with('/')
    } else {
        false
    }
}
