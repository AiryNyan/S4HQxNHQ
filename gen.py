# Generates stubs.s / table.rs / d3d11.def from the system d3d11.dll export list.
import pefile
pe = pefile.PE(r"C:\Windows\System32\d3d11.dll", fast_load=True)
pe.parse_data_directories(directories=[pefile.DIRECTORY_ENTRY['IMAGE_DIRECTORY_ENTRY_EXPORT']])
names = sorted(e.name.decode() for e in pe.DIRECTORY_ENTRY_EXPORT.symbols if e.name)
own = {"D3D11CreateDevice", "D3D11CreateDeviceAndSwapChain"}
fwd = [n for n in names if n not in own]
with open("src/stubs.s", "w") as f:
    for i, n in enumerate(fwd):
        f.write(f".globl {n}\n{n}:\n    jmp qword ptr [rip + HQX_TABLE + {8*i}]\n")
with open("src/table.rs", "w") as f:
    f.write(f"const N: usize = {len(fwd)};\nconst NAMES: [&[u8]; N] = [\n")
    for n in fwd: f.write(f'    b"{n}\\0",\n')  # escape, not a raw NUL byte
    f.write("];\n")
with open("d3d11.def", "w") as f:
    f.write("EXPORTS\n" + "".join(n + "\n" for n in names))
print(len(names), "exports;", len(fwd), "forwarded")
