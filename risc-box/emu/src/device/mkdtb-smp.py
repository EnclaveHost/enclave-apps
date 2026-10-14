#!/usr/bin/env python3
"""risc-box (SMP): dtb.dts -> dtb-smp<N>.dtb, the same machine with N harts. Each hart gets a cpu node and its own
interrupt controller; the PLIC lists every hart's (M external 11, S external 9) contexts and the CLINT every hart's
(M software 3, M timer 7), in hart order - context 2h is hart h's M mode, 2h+1 its S mode, as the emulator's PLIC
decodes them. Usage: mkdtb-smp.py N...   (needs dtc)"""
import re, subprocess, sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
src = (HERE / 'dtb.dts').read_text()

def build(n):
    cpu = lambda h: 0x10 + 2 * h      # cpu node phandle
    intc = lambda h: 0x11 + 2 * h     # its interrupt controller
    cores = ''.join(f'\t\t\t\tcore{h} {{\n\t\t\t\t\tcpu = <{cpu(h):#04x}>;\n\t\t\t\t}};\n' for h in range(n))
    nodes = ''.join(f'''
		cpu@{h} {{
			phandle = <{cpu(h):#04x}>;
			device_type = "cpu";
			reg = <{h:#04x}>;
			status = "okay";
			compatible = "riscv";
			riscv,isa = "rv64imafdcsu";
			mmu-type = "riscv,sv39";

			interrupt-controller {{
				#interrupt-cells = <0x01>;
				interrupt-controller;
				compatible = "riscv,cpu-intc";
				phandle = <{intc(h):#04x}>;
			}};
		}};
''' for h in range(n))
    cpus = f'''	cpus {{
		#address-cells = <0x01>;
		#size-cells = <0x00>;
		timebase-frequency = <0x989680>;

		cpu-map {{

			cluster0 {{

{cores}			}};
		}};
{nodes}	}};
'''
    s = re.sub(r'\tcpus \{.*?\n\t\};\n', lambda m: cpus, src, count=1, flags=re.S)
    plic = ' '.join(f'{intc(h):#04x} 0x0b {intc(h):#04x} 0x09' for h in range(n))
    clint = ' '.join(f'{intc(h):#04x} 0x03 {intc(h):#04x} 0x07' for h in range(n))
    assert s.count('interrupts-extended = <0x02 0x0b 0x02 0x09>;') == 1
    assert s.count('interrupts-extended = <0x02 0x03 0x02 0x07>;') == 1
    s = s.replace('interrupts-extended = <0x02 0x0b 0x02 0x09>;', f'interrupts-extended = <{plic}>;')
    s = s.replace('interrupts-extended = <0x02 0x03 0x02 0x07>;', f'interrupts-extended = <{clint}>;')
    out = HERE / f'dtb-smp{n}.dtb'
    subprocess.run(['dtc', '-q', '-I', 'dts', '-O', 'dtb', '-o', str(out), '-'], input=s.encode(), check=True)
    print(out.name, out.stat().st_size)

for a in sys.argv[1:]:
    build(int(a))
