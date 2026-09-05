// Appended to cpu.rs only by aot_verifier.py's isolated fixture build.
// Both fixture regions contain ADDI x1, x0, 1, at ordinary/kernel addresses.
pub fn aot_verifier_regression_probe() {
    fn write_phys(cpu: &mut Cpu, address: u64, bytes: &[u8]) {
        for (i, byte) in bytes.iter().enumerate() {
            cpu.mmu.store_raw(address + i as u64, *byte);
        }
    }
    for (handle, virtual_pc) in [(0u32, 0x8000_0000u64), (1, 0xffff_ffc0_0000_0000)] {
        let mut cpu = Cpu::new(Box::new(::terminal::DummyTerminal::new()));
        cpu.mmu.init_memory(65536);
        cpu.update_xlen(Xlen::Bit64);
        let root = ::mmu::DRAM_BASE + 0x1000;
        let middle = ::mmu::DRAM_BASE + 0x2000;
        let leaf = ::mmu::DRAM_BASE + 0x3000;
        let bad = ::mmu::DRAM_BASE + 0x4000;
        let good = ::mmu::DRAM_BASE + 0x5000;
        let pte = |physical: u64, flags: u64| ((physical >> 12) << 10) | flags;
        write_phys(&mut cpu, good, &0x00100093u32.to_le_bytes());
        write_phys(&mut cpu, root + ((virtual_pc >> 30) & 511) * 8, &pte(middle, 1).to_le_bytes());
        write_phys(&mut cpu, middle + ((virtual_pc >> 21) & 511) * 8, &pte(leaf, 1).to_le_bytes());
        let leaf_entry = leaf + ((virtual_pc >> 12) & 511) * 8;
        write_phys(&mut cpu, leaf_entry, &pte(bad, 0xcf).to_le_bytes());
        cpu.mmu.update_addressing_mode(::mmu::AddressingMode::SV39);
        cpu.mmu.update_ppn(root >> 12);
        cpu.mmu.update_privilege_mode(PrivilegeMode::Supervisor);
        cpu.aot_enable();
        assert!(!cpu.aot_verified(handle), "mismatching instructions must fail");
        let generation = cpu.mmu.code_gen();
        cpu.mmu.update_ppn(root >> 12);
        assert_eq!(generation, cpu.mmu.code_gen());
        assert!(!cpu.aot_verified(handle), "mapping equality must not promote a failed instruction proof");

        // Page-table writes do not modify either code page: only tlb_gen
        // changes. A new mapping still needs its instruction bytes checked.
        write_phys(&mut cpu, leaf_entry, &pte(good, 0xcf).to_le_bytes());
        cpu.mmu.update_ppn(root >> 12);
        assert_eq!(generation, cpu.mmu.code_gen());
        assert!(cpu.aot_verified(handle), "matching instructions on a new physical page must be accepted");
        write_phys(&mut cpu, leaf_entry, &pte(bad, 0xcf).to_le_bytes());
        cpu.mmu.update_ppn(root >> 12);
        assert_eq!(generation, cpu.mmu.code_gen());
        assert!(!cpu.aot_verified(handle), "remapping verified code to mismatching bytes must fail, including kernel addresses");

        write_phys(&mut cpu, bad, &0x00100093u32.to_le_bytes());
        assert_ne!(generation, cpu.mmu.code_gen());
        assert!(cpu.aot_verified(handle), "a real code change to matching instructions must be rechecked");
        cpu.mmu.update_ppn(root >> 12);
        assert!(cpu.aot_verified(handle), "an unchanged successful proof remains valid");
        println!("AOT verifier PASS for {virtual_pc:#x}");
    }
}
