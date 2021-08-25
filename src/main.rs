use std::cmp;
use std::fmt::Write as fmtwrite;
use std::fs;
use std::io;
use std::io::{Seek, Write};
use std::mem;
use structopt::StructOpt;
use util::{align_to, amount_alignment_needed};

mod cmdline;
mod header;
mod util;

fn main() {
    let opt = cmdline::Opt::from_args();

    let package_name = opt
        .package_name
        .as_ref()
        .map_or("", |package_name| package_name.as_str());

    // If kernel_major is set, the app requires kernel ^kernel_major.0 (>=
    // kernel_major.0, < (kernel_major+1).0) Optionally, kernel_minor can be
    // set, making the app require ^kernel_major.kernel_minor (>=
    // kernel_major.kernel_minor, < (kernel_major+1).0).
    let minimum_tock_kernel_version = match opt.kernel_major {
        Some(major) => Some((major, opt.kernel_minor.unwrap_or(0))),
        None => None,
    };

    // Create the metadata.toml file needed for the TAB file.
    let mut metadata_toml = String::new();
    // TAB version is currently "1". This defines the general format, but
    // key-value pairs can be added (or removed) and still be version 1.
    writeln!(&mut metadata_toml, "tab-version = 1").unwrap();
    // Name is always set by elf2tab (even if it is empty).
    writeln!(&mut metadata_toml, "name = \"{}\"", package_name).unwrap();
    // We don't currently tell elf2tab if this app only runs on certain boards.
    writeln!(&mut metadata_toml, "only-for-boards = \"\"").unwrap();
    // Include "minimum-tock-kernel-version" key if a necessary kernel version
    // was specified.
    minimum_tock_kernel_version.map(|(major, minor)| {
        writeln!(
            &mut metadata_toml,
            "minimum-tock-kernel-version = \"{}.{}\"",
            major, minor
        )
        .unwrap();
    });
    // Add build-date metadata unless a deterministic build is desired.
    if !opt.deterministic {
        let build_date = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        writeln!(&mut metadata_toml, "build-date = {}", build_date).unwrap();
    }

    // Start creating a tar archive which will be the .tab file.
    let tab_name = fs::File::create(&opt.output).expect("Could not create the output file.");
    let mut tab = tar::Builder::new(tab_name);
    tab.mode(tar::HeaderMode::Deterministic);

    // Add the metadata file without creating a real file on the filesystem.
    let mut header = tar::Header::new_gnu();
    header.set_size(metadata_toml.as_bytes().len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tab.append_data(&mut header, "metadata.toml", metadata_toml.as_bytes())
        .unwrap();

    // Iterate all input elfs. Convert them to Tock friendly binaries and then
    // add them to the TAB file.
    for elf_path in opt.input {
        let tbf_path = elf_path.with_extension("tbf");

        let elffile = elf::File::open_path(&elf_path).expect("Could not open the .elf file.");

        if opt.output.clone() == tbf_path.clone() {
            panic!(
                "tab file {} and output file {} cannot be the same file",
                opt.output.clone().to_str().unwrap(),
                tbf_path.to_str().unwrap()
            );
        }

        // Get output file as both read/write for creating the binary and
        // adding it to the TAB tar file.
        let mut outfile: fs::File = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(tbf_path.clone())
            .unwrap();

        // Do the conversion to a tock binary.
        if opt.verbose {
            println!("Creating {:?}", tbf_path);
        }
        elf_to_tbf(
            &elffile,
            &mut outfile,
            opt.package_name.clone(),
            opt.verbose,
            opt.stack_size,
            opt.app_heap_size,
            opt.kernel_heap_size,
            opt.protected_region_size,
            opt.permissions.to_vec(),
            minimum_tock_kernel_version,
        )
        .unwrap();
        if opt.verbose {
            println!("");
        }

        // Add the file to the TAB tar file.
        outfile.seek(io::SeekFrom::Start(0)).unwrap();
        tab.append_file(tbf_path.file_name().unwrap(), &mut outfile)
            .unwrap();
        outfile.seek(io::SeekFrom::Start(0)).unwrap();
        tab.append_file(
            tbf_path.with_extension("bin").file_name().unwrap(),
            &mut outfile,
        )
        .unwrap();
    }
}

/// Convert an ELF file to a TBF (Tock Binary Format) binary file.
///
/// This will place all writeable and executable sections from the ELF file
/// into a binary and prepend a TBF header to it. For all writeable sections,
/// if there is a .rel.X section it will be included at the end with a 32 bit
/// length parameter first.
///
/// Assumptions:
/// - Any segments that are writable and set to be loaded into flash but with a
///   different virtual address will be in RAM and should count towards minimum
///   required RAM.
/// - Sections that are writeable flash regions include .wfr in their name.
fn elf_to_tbf<W: Write>(
    input: &elf::File,
    output: &mut W,
    package_name: Option<String>,
    verbose: bool,
    stack_len: u32,
    app_heap_len: u32,
    kernel_heap_len: u32,
    protected_region_size_arg: Option<u32>,
    permissions: Vec<(u32, u32)>,
    kernel_version: Option<(u16, u16)>,
) -> io::Result<()> {
    let package_name = package_name.unwrap_or_default();

    // Get an array of the sections sorted so we place them in the proper order
    // in the binary.
    let mut sections_sort: Vec<(usize, usize)> = Vec::new();
    for (i, section) in input.sections.iter().enumerate() {
        sections_sort.push((i, section.shdr.offset as usize));
    }
    sections_sort.sort_by_key(|s| s.1);

    // Keep track of how much RAM this app will need.
    let mut minimum_ram_size: u32 = 0;

    // Find all segments destined for the RAM section that are stored in flash.
    // These are set in the linker file to consume memory, and we need to
    // account for them when we set the minimum amount of memory this app
    // requires.
    for segment in &input.phdrs {
        // To filter, we need segments that are:
        // - Set to be LOADed.
        // - Have different virtual and physical addresses, meaning they are
        //   loaded into flash but actually reside in memory.
        // - Are not zero size in memory.
        // - Are writable (RAM should be writable).
        if segment.progtype == elf::types::PT_LOAD
            && segment.vaddr != segment.paddr
            && segment.memsz > 0
            && ((segment.flags.0 & elf::types::PF_W.0) > 0)
        {
            minimum_ram_size += segment.memsz as u32;
        }
    }
    if verbose {
        println!(
            "Min RAM size from segments in ELF: {} bytes",
            minimum_ram_size
        );
    }

    // Add in room the app is asking us to reserve for the stack and heaps to
    // the minimum required RAM size.
    minimum_ram_size +=
        align_to(stack_len, 8) + align_to(app_heap_len, 4) + align_to(kernel_heap_len, 4);

    // Check for fixed addresses.
    //
    // We do this with different mechanisms for flash and ram. For flash, we
    // look at loadable segments in the ELF, and find the segment with the
    // lowest address that is both executable and non-zero in file size. Since
    // this is a segment that must be loaded to execute the application, we know
    // that this must be a flash address.
    //
    // For RAM, we can't quite use the ELF file in the same way as with flash.
    // Since nothing _actually_ has to be loaded into RAM, the ELF file does not
    // have to keep track of the first address in RAM. Further, Tock apps
    // typically put a .stack section at the beginning of RAM, and the stack is
    // just a memory holder and doesn't contain any actual data. The start of
    // RAM address will almost certainly exist somewhere in the ELF, but
    // reliably extracting it from different ELFs linked with different
    // toolchains from different linker scripts would I think require resorting
    // to a bit of heuristics and guessing. To avoid the potential issues there,
    // we instead require that a `_sram_origin` symbol be present to explicitly
    // mark the start of RAM.
    //
    // In both cases we check to see if the address matches our expected PIC
    // addresses:
    // - RAM: 0x00000000
    // - flash: 0x80000000
    //
    // These addresses are a Tock convention and enables PIC fixups to be done
    // by the app when it first starts. If for some reason an app is PIC and
    // wants to use different dummy PIC addresses, then this logic will have to
    // be updated.
    let mut fixed_address_flash: Option<u32> = None;
    let mut fixed_address_ram: Option<u32> = None;
    let mut fixed_address_flash_pic: bool = false;

    /// Helper function to determine if any nonzero length section is inside a
    /// given segment.
    ///
    /// This is necessary because we sometimes run into loadable segments that
    /// shouldn't really exist (they are at addresses outside of what was
    /// specified in the linker script), and we want to be able to skip them.
    fn section_exists_in_segment(input: &elf::File, segment: &elf::types::ProgramHeader) -> bool {
        let segment_start = segment.offset as u32;
        let segment_size = segment.filesz as u32;
        let segment_end = segment_start + segment_size;

        for section in input.sections.iter() {
            let section_start = section.shdr.offset as u32;
            let section_size = section.shdr.size as u32;
            let section_end = section_start + section_size;

            if section_start >= segment_start && section_end <= segment_end && section_size > 0 {
                return true;
            }
        }
        false
    }

    // Do flash address.
    for segment in &input.phdrs {
        match segment.progtype {
            elf::types::PT_LOAD => {
                // Look for segments based on their flags, size, and whether
                // they actually contain any valid sections. Flash segments have
                // to be marked executable, and we only care about segments that
                // actually contain data to be loaded into flash.
                if (segment.flags.0 & elf::types::PF_X.0) > 0
                    && segment.filesz > 0
                    && section_exists_in_segment(input, segment)
                {
                    // If this is standard Tock PIC, then this virtual address
                    // will be at 0x80000000. Otherwise, we interpret this to
                    // mean that the binary was compiled for a fixed address in
                    // flash.
                    if segment.vaddr == 0x80000000 {
                        fixed_address_flash_pic = true;
                    } else {
                        fixed_address_flash = if let Some(prev_addr) = fixed_address_flash {
                            Some(cmp::min(prev_addr, segment.vaddr as u32))
                        } else {
                            Some(segment.vaddr as u32)
                        }
                    }
                }
            }

            _ => {}
        }
    }
    // Use the flags to see if we got PIC sections, and clear any other fixed
    // addresses we may have found.
    if fixed_address_flash_pic {
        fixed_address_flash = None;
    }

    // Do RAM address.
    // Get all symbols in the symbol table section if it exists.
    let section_symtab = input.sections.iter().find(|s| s.shdr.name == ".symtab");
    section_symtab.map(|s_symtab| {
        let symbols = input.get_symbols(s_symtab);
        symbols.ok().map(|syms| {
            // We are looking for the `_sram_origin` symbol and its value.
            // If it exists, we try to use it. Otherwise, we just do not try
            // to find a fixed RAM address.
            let sram_origin_symbol = syms.iter().find(|sy| sy.name == "_sram_origin");
            sram_origin_symbol.map(|sram_origin| {
                let sram_origin_address = sram_origin.value as u32;
                // If address does not match our dummy address for PIC, then we
                // say this app has a fixed address for memory.
                if sram_origin_address != 0x00000000 {
                    fixed_address_ram = Some(sram_origin_address);
                }
            });
        });
    });

    // Need an array of sections to look for relocation data to include.
    let mut rel_sections: Vec<String> = Vec::new();

    // Iterate the sections in the ELF file to find properties of the app that
    // are required to go in the TBF header.
    let mut writeable_flash_regions_count = 0;

    for s in &sections_sort {
        let section = &input.sections[s.0];

        // Count write only sections as writeable flash regions.
        if section.shdr.name.contains(".wfr") && section.shdr.size > 0 {
            writeable_flash_regions_count += 1;
        }

        // Check write+alloc sections for possible .rel.X sections.
        if section.shdr.flags.0 == elf::types::SHF_WRITE.0 + elf::types::SHF_ALLOC.0 {
            // This section is also one we might need to include relocation
            // data for.
            rel_sections.push(section.shdr.name.clone());
        }
    }
    if verbose {
        println!(
            "Number of writeable flash regions: {}",
            writeable_flash_regions_count
        );
    }

    if verbose {
        if let Some((major, minor)) = kernel_version {
            println!("Kernel version: {}.{}", major, minor);
        }
    }

    // Keep track of an index of where we are in creating the app binary.
    let mut binary_index = 0;

    // Now we can create the first pass TBF header. This is mostly to get the
    // size of the header since we have to fill in some of the offsets later.
    let mut tbfheader = header::TbfHeader::new();
    let header_length = tbfheader.create(
        minimum_ram_size,
        writeable_flash_regions_count,
        package_name,
        fixed_address_ram,
        fixed_address_flash,
        permissions,
        kernel_version,
    );
    // If a protected region size was passed, confirm the header will fit.
    // Otherwise, use the header size as the protected region size.
    let protected_region_size =
        if let Some(fixed_protected_region_size) = protected_region_size_arg {
            if fixed_protected_region_size < header_length as u32 {
                // The header doesn't fit in the provided protected region size;
                // throw an error.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                    "protected_region_size = {} is too small for the TBF headers. Header size: {}",
                    fixed_protected_region_size, header_length),
                ));
            }
            // Update the header's protected size, as the protected region may
            // be larger than the header size.
            tbfheader.set_protected_size(fixed_protected_region_size - header_length as u32);

            fixed_protected_region_size
        } else {
            // The protected region was not specified on the command line.
            // Normally, we default to an additional size of 0 for the protected
            // region beyond the header. However, if we are _not_ doing PIC, we
            // might want to choose a nonzero sized protected region. Without
            // PIC, the application binary must be at specific address. In
            // addition, boards have a fixed address where they start looking
            // for applications. To make both of those addresses match, we can
            // expand the protected region.
            //
            // |----------------------|-------------------|---------------------
            // | TBF Header           | Protected Region  | Application Binary
            // |----------------------|-------------------|---------------------
            // ^                                ^         ^
            // |                                |         |-- Fixed binary address
            // |-- Start of apps address        |-- Flexible size
            //
            // However, we don't actually know the start of apps address.
            // Additionally, an app may be positioned after another app in
            // flash, and so the start address is actually the start of apps
            // address plus the size of the first app. Tockloader when it goes
            // to actually load the app can check for these addresses and expand
            // the protected region as needed. But, in some cases it is easier
            // to just be able to flash the TBF directly onto the board without
            // needing Tockloader. So, we at least try to pick a reasonable
            // protected size in the non-PIC case to give the TBF a chance of
            // working as created.
            //
            // So, we put the start address of the TBF header at an alignment of
            // 256 if the application binary is at the expected address.
            if !fixed_address_flash_pic {
                // Non-PIC case. As a reasonable guess we try to get our TBF
                // start address to be at a 256 byte alignment.
                let app_binary_address = fixed_address_flash.unwrap_or(0); // Already checked for `None`.
                let tbf_start_address = util::align_down(app_binary_address, 256);
                let protected_region_size = app_binary_address - tbf_start_address;
                if protected_region_size > header_length as u32 {
                    // We do want to set the protected size past the header to
                    // something nonzero.
                    if verbose {
                        println!(
                            "Inserting nonzero protected region of length: {} bytes",
                            protected_region_size - header_length as u32
                        );
                    }
                    tbfheader.set_protected_size(protected_region_size - header_length as u32);
                    protected_region_size
                } else {
                    header_length as u32
                }
            } else {
                // Normal PIC case, no need to insert extra protected region.
                header_length as u32
            }
        };
    binary_index += protected_region_size as usize;

    // The init function is where the app will start executing, defined as an
    // offset from the end of protected region at the beginning of the app in
    // flash. Typically the protected region only includes the TBF header. To
    // calculate the offset we need to find which section includes the entry
    // function and then determine its offset relative to the end of the
    // protected region.
    let mut init_fn_offset: u32 = 0;

    // Need a place to put the app sections before we know the true TBF header.
    let mut binary: Vec<u8> = vec![0; protected_region_size as usize - header_length];

    let mut entry_point_found = false;

    // Iterate the sections in the ELF file and add them to the binary as needed
    for s in &sections_sort {
        let section = &input.sections[s.0];

        // Determine if this is the section where the entry point is in. If it
        // is, then we need to calculate the correct init_fn_offset.
        if input.ehdr.entry >= section.shdr.addr
            && input.ehdr.entry < (section.shdr.addr + section.shdr.size)
            && (section.shdr.name.find("debug")).is_none()
        {
            // panic in case we detect entry point in multiple sections.
            if entry_point_found {
                panic!("Duplicate entry point in {} section", section.shdr.name);
            }
            entry_point_found = true;

            if verbose {
                println!("Entry point is in {} section", section.shdr.name);
            }
            // init_fn_offset is specified relative to the end of the TBF
            // header.
            init_fn_offset = (input.ehdr.entry - section.shdr.addr) as u32
                + (binary_index - header_length) as u32
        }

        // If this is writeable, executable, or allocated, is nonzero length,
        // and is type `PROGBITS` we want to add it to the binary.
        if (section.shdr.flags.0
            & (elf::types::SHF_WRITE.0 + elf::types::SHF_EXECINSTR.0 + elf::types::SHF_ALLOC.0)
            != 0)
            && section.shdr.shtype == elf::types::SHT_PROGBITS
            && section.shdr.size > 0
        {
            if verbose {
                println!(
                    "  Adding {0} section. Offset: {1} ({1:#x}). Length: {2} ({2:#x}) bytes.",
                    section.shdr.name,
                    binary_index,
                    section.data.len(),
                );
            }
            if amount_alignment_needed(binary_index as u32, 4) != 0 {
                println!(
                    "Warning! Placing section {} at {:#x}, which is not 4-byte aligned.",
                    section.shdr.name, binary_index
                );
            }
            binary.extend(&section.data);

            // Check if this is a writeable flash region. If so, we need to
            // set the offset and size in the header.
            if section.shdr.name.contains(".wfr") && section.shdr.size > 0 {
                tbfheader.set_writeable_flash_region_values(
                    binary_index as u32,
                    section.shdr.size as u32,
                );
            }

            // Now increment where we are in the binary.
            binary_index += section.shdr.size as usize;
        }
    }

    // Now that we have checked all of the sections, we can set the
    // init_fn_offset.
    tbfheader.set_init_fn_offset(init_fn_offset);

    // Next we have to add in any relocation data.
    let mut relocation_binary: Vec<u8> = Vec::new();

    // For each section that might have relocation data, check if a .rel.X
    // section exists and if so include it.
    if verbose {
        println!("Searching for .rela.X sections to add.");
    }
    for relocation_section_name in &rel_sections {
        let mut name: String = ".rela".to_owned();
        name.push_str(relocation_section_name);

        let rel_data = input
            .sections
            .iter()
            .find(|section| section.shdr.name == name)
            .map_or(&[] as &[u8], |section| section.data.as_ref());

        relocation_binary.extend(rel_data);

        if verbose && !rel_data.is_empty() {
            println!(
                "  Adding {0} section. Offset: {1} ({1:#x}). Length: {2} ({2:#x}) bytes.",
                name,
                binary_index + mem::size_of::<u32>() + rel_data.len(),
                rel_data.len(),
            );
        }
        if !rel_data.is_empty() && amount_alignment_needed(binary_index as u32, 4) != 0 {
            println!(
                "Warning! Placing section {} at {:#x}, which is not 4-byte aligned.",
                name, binary_index
            );
        }
    }

    // Add the relocation data to our total length. Also include the 4 bytes for
    // the relocation data length.
    binary_index += relocation_binary.len() + mem::size_of::<u32>();

    // That is everything that we are going to include in our app binary. Now
    // we need to pad the binary to a power of 2 in size, and make sure it is
    // at least 512 bytes in size.
    let post_content_pad = if binary_index.count_ones() > 1 {
        let power2len = cmp::max(1 << (32 - (binary_index as u32).leading_zeros()), 512);
        power2len - binary_index
    } else {
        0
    };
    binary_index += post_content_pad;
    let total_size = binary_index;

    // Now set the total size of the app in the header.
    tbfheader.set_total_size(total_size as u32);

    if verbose {
        print!("{}", tbfheader);
    }

    // Write the header and actual app to a binary file.
    output.write_all(tbfheader.generate().unwrap().get_ref())?;
    output.write_all(binary.as_ref())?;

    let rel_data_len: [u8; 4] = (relocation_binary.len() as u32).to_le_bytes();
    output.write_all(&rel_data_len)?;
    output.write_all(relocation_binary.as_ref())?;

    // Pad to get a power of 2 sized flash app.
    util::do_pad(output, post_content_pad as usize)?;

    Ok(())
}
