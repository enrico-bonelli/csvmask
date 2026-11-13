# csvmask

Programma CLI scritto in Rust per mascherare ad alte prestazioni i caratteri di colonne specifiche (indicizzate da 1) in file CSV.

## Caratteristiche principali

- **Zero dipendenze esterne** (solo libreria standard Rust).
- **Molto performante**:
  - Scansione SIMD (SSE2 su x86_64, fallback SWAR portabile): maschere di bit di delimitatori, virgolette e fine riga calcolate a blocchi di 64 byte, così il parser salta da un carattere strutturale al successivo senza una nuova ricerca per ogni campo.
  - Copia differita delle colonne non mascherate (un'unica copia per tratto contiguo invece che per campo) e salto immediato del resto della riga dopo l'ultima colonna da mascherare.
  - Pipeline multi-thread con pool di buffer riutilizzabili da 2 MiB (zero allocazioni a regime, memoria limitata, ordine garantito delle righe); i blocchi vengono elaborati in parallelo anche quando un campo tra virgolette su più righe attraversa il confine tra due blocchi.
  - Percorso rapido per campi ASCII e supporto completo UTF-8, campi tra virgolette (`"..."`), virgolette escape (`""`), campi multi-linea e terminatori `\n` / `\r\n`.

## Compilazione

```bash
cargo build --release
```

Il binario compilato si trova in `target/release/csvmask`.

## Utilizzo

```bash
# Sintassi base (scrive su stdout, separatore default ';', maschera default '*')
csvmask input.csv 1,3,5

# Scrittura su file di output e preservazione dell'intestazione (-H)
csvmask input.csv 2,4 -H -o output.csv

# Separatore personalizzato e carattere di mascheramento personalizzato
csvmask input.csv 1,3-5 -d ',' -m '#' -o output.csv

# Lettura da stdin
cat input.csv | csvmask - 2 > output.csv
```

### Opzioni

- `-i, --input <FILE>`: File CSV in input (`-` per stdin)
- `-c, --columns <COLS>`: Lista di colonne 1-based (es. `1,3,5` oppure intervalli `2-4`)
- `-o, --output <FILE>`: File CSV in output (default: stdout)
- `-d, --delimiter <CHAR>` / `-s, --separator <CHAR>`: Separatore CSV (default: `;`; supporta `\t` o `tab`)
- `-m, --mask-char <CHAR>` / `--mask <CHAR>`: Carattere di mascheramento (default: `*`)
- `-H, --keep-header`: Non maschera la prima riga (intestazione)
- `--keep-start <N>`: Numero di caratteri iniziali da lasciare visibili
- `--keep-end <N>`: Numero di caratteri finali da lasciare visibili
- `-j, --threads <N>`: Numero di worker thread (default: automatico in base ai core CPU)

## Regole di mascheramento predefinite

Per ciascun campo nelle colonne selezionate (in base al numero di caratteri Unicode):
- Lunghezza `0`: invariato (`""`)
- Lunghezza `1`: `*`
- Lunghezza `2`: `a*` (primo carattere visibile, secondo mascherato)
- Lunghezza `>= 3`: `M***o` (primo e ultimo carattere visibili, caratteri centrali mascherati; personalizzabile con `--keep-start` e `--keep-end`)

## Licenza

Distribuito con licenza BSD 3-Clause: vedi [LICENSE.txt](LICENSE.txt).
