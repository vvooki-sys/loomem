<span id="backup-and-restore"></span>

# Kopia zapasowa i przywracanie

<span id="what-is-backed-up"></span>

## Co obejmuje kopia zapasowa

Checkpoint RocksDB to migawka wszystkich danych z danego momentu:

- Wszystkie fragmenty (L0, L1)
- Embeddingi (przechowywane w rodzinie kolumn RocksDB)
- Graf wiedzy (encje, krawędzie)
- Dane o kosztach
- Zabezpieczone klucze szyfrowania poszczególnych strumieni (gdy szyfrowanie danych w spoczynku jest włączone)

**Poza checkpointem RocksDB:**

- Indeks pełnotekstowy Tantivy. Niezgodność schematu lub liczby rekordów może uruchomić odbudowę, ale nie dowodzi zgodności istniejącego indeksu z tym checkpointem. Po przywróceniu uruchom odbudowę ręcznie.
- Osobny dziennik operacji Loomem (intent log), zwykle `{data_dir}/wal`. To inny dziennik niż WAL RocksDB wewnątrz checkpointu. Odtworzenie dziennika z innego momentu może zmienić przywrócone dane; zastosuj procedurę poniżej.

<span id="encrypted-instance-backup-is-joint-master-key-required"></span>

## Kopia zaszyfrowanej instancji wymaga obu elementów (w tym klucza głównego)

Gdy szyfrowanie danych w spoczynku jest włączone (ustawione `LOOMEM_AT_REST_MASTER_KEY`), checkpoint zawiera tylko **zabezpieczone** klucze DEK dla poszczególnych zakresów (w rodzinie kolumn `keys`) oraz **szyfrogram** dla szyfrowanych klas rekordów (treść fragmentów, encje, relacje, encje grafu i audyt). **Sam checkpoint** RocksDB nie wystarczy do ich odszyfrowania — nie zawiera klucza głównego odblokowującego DEK.

Użyteczna kopia zapasowa zaszyfrowanej instancji wymaga więc **obu elementów**:

> **checkpointu** oraz **potwierdzonej kopii klucza głównego**, którego **odcisk odpowiada** danym.

- **Zapisz odcisk obok każdego checkpointu.** Pobierz go podczas tworzenia kopii, aby przy przywracaniu ustalić, którym zabezpieczonym kluczem zaszyfrowano dane. Zastąp przykładową ścieżkę katalogiem ukończonego checkpointu. Przykład używa `LOOMEM_AUTH_TOKEN` do uwierzytelnienia; jeśli jest ono wyłączone, pomiń nagłówek:

```bash
LOOMEM_CHECKPOINT_DIR="/path/to/completed-checkpoint"
set -o pipefail
fingerprint=$(curl -fsS http://localhost:3030/v1/encryption/status \
  -H "Authorization: Bearer ${LOOMEM_AUTH_TOKEN:?set the admin token}" \
  | jq -er '.master_key_fingerprint | strings | select(length > 0)') &&
  printf '%s\n' "$fingerprint" > "$LOOMEM_CHECKPOINT_DIR/master-key-fingerprint.txt"
```

Odcisk jest jednokierunkowym skrótem i można go bezpiecznie przechowywać jawnym tekstem obok checkpointu. Sam klucz **nigdy** nie jest tam przechowywany; znajduje się wyłącznie w bezpiecznym depozycie.

- **Dotyczy to również automatycznych checkpointów tworzonych co 12 godzin.** Każdy checkpoint utworzony przy użyciu tego samego klucza głównego ma ten sam odcisk. Zapis depozytu (klucz, odcisk i data aktywacji) pozwala na podstawie czasu wskazać właściwy klucz dla **dowolnego** checkpointu, także takiego bez osobnego pliku z odciskiem. Powyższy krok z `master-key-fingerprint.txt` ułatwia szybkie wyszukanie, ale nie jest warunkiem odzyskania danych. Jego automatyzacja po utworzeniu kopii może być przyszłym usprawnieniem, a nie brakującym warunkiem odzyskiwania.

- **Bez odpowiedniego klucza głównego zaszyfrowanego checkpointu nie można odzyskać.** Metadane routingu, embeddingi i Tantivy pozostają czytelne, ale wszystkie szyfrowane klasy rekordów pozostają niedostępne.

Przechowuj przynajmniej jedną kopię klucza głównego wraz z odciskiem i datą aktywacji w bezpiecznym miejscu **oddzielonym od wolumenu danych i kopii zapasowych**, np. w menedżerze haseł, menedżerze sekretów w chmurze lub w drukowanym depozycie. Sprawdź, czy odcisk klucza w depozycie odpowiada odciskowi działającej instancji (`GET /v1/encryption/status`). Rotacja klucza głównego nie jest obecnie obsługiwana: zmiana klucza nie zabezpiecza ponownie istniejących DEK i uniemożliwia odczyt zaszyfrowanych danych. Zobacz [przegląd bezpieczeństwa](SECURITY.md).

<span id="where-backups-are-stored"></span>

## Gdzie są przechowywane kopie zapasowe

``` text
{data_dir}/backups/checkpoint-{YYYY-MM-DD-HHMMSS}/
```

- **Lokalnie:** `./data/backups/`
- **Wdrożenia kontenerowe:** `/data/backups/` (na wolumenie trwałym)

<span id="configuration"></span>

## Konfiguracja

``` toml
[worker.backup]
enabled = true
interval_secs = 43200    # co 12 godzin
max_copies = 2           # zachowaj 2 ostatnie (1 dzień)
```

<span id="manual-backup"></span>

## Ręczne tworzenie kopii

Aby utworzyć kopię zewnętrzną, skopiuj ukończony katalog `{data_dir}/backups/checkpoint-*`. Checkpoint jest kopią RocksDB z danego momentu; sam w sobie nie gwarantuje spójności z osobnym dziennikiem operacji.

Checkpoint RocksDB nie jest transakcyjnie sparowaną migawką osobnego dziennika operacji Loomem. Automatyczny checkpoint może powstać w trakcie niezakończonego usuwania lub czyszczenia danych. Sam checkpoint nie dowodzi więc ukończenia tych operacji. Po przywróceniu bez pasującego dziennika sprawdź, czy oczekiwane usunięcia zostały wykonane, zanim ponownie podłączysz klientów.

<span id="restore-procedure"></span>

## Procedura przywracania

<span id="1-stop-loomem"></span>

### 1. Zatrzymaj Loomem

Odłącz klientów i całkowicie zatrzymaj serwer. Na czas przywracania nie podłączaj klientów i ustaw tymczasowo `enabled = false` w sekcji `[scheduler]` konfiguracji. Przywróć zwykły dostęp i ustawienia schedulera dopiero po weryfikacji.

``` bash
pkill -f loomem-server
```

<span id="2-replace-rocksdb-data"></span>

### 2. Zastąp dane RocksDB

``` bash
# Odłóż obecne dane na bok
mv {data_dir}/rocksdb {data_dir}/rocksdb.old

# Skopiuj checkpoint
cp -r {data_dir}/backups/checkpoint-2026-04-04-120000 {data_dir}/rocksdb
```

<span id="2b-set-aside-the-separate-intent-log"></span>

### 2b. Odłóż osobny dziennik operacji

Przy zatrzymanym serwerze przenieś istniejący katalog dziennika operacji w bezpieczne miejsce poza aktywnym katalogiem danych, pod unikalną nazwą. Domyślna ścieżka to `{data_dir}/wal`, ale sprawdź `[storage.intent_log].dir` w używanej konfiguracji. Nie używaj starego dziennika z tym checkpointem: oczekujące operacje usuwania mogą przy starcie zmienić przywrócone dane RocksDB. Zachowaj stary dziennik do analizy; włączony serwer utworzy pusty. **Nie** usuwaj plików WAL samego RocksDB wewnątrz checkpointu.

<span id="3-encrypted-instances-confirm-the-master-key-is-set"></span>

### 3. (Instancje szyfrowane) Sprawdź, czy ustawiono klucz główny

Jeśli checkpoint pochodzi z zaszyfrowanej instancji, odpowiedni klucz główny musi być dostępny **przed** uruchomieniem; w przeciwnym razie zaszyfrowane rekordy pozostaną niedostępne. Przywróć `LOOMEM_AT_REST_MASTER_KEY` z depozytu i sprawdź, czy jego odcisk odpowiada odciskowi z pliku `master-key-fingerprint.txt` zapisanego obok checkpointu (po uruchomieniu `GET /v1/encryption/status` zwraca aktywny odcisk). W przypadku instancji bez szyfrowania pomiń ten krok.

<span id="4-start-loomem"></span>

### 4. Uruchom Loomem

``` bash
./target/release/loomem-server
```

RocksDB otworzy dane z checkpointu. Uruchomienie może spowodować odbudowę Tantivy przy niezgodności schematu, ustawionej fladze odbudowy lub wykrytej różnicy liczby rekordów, ale żaden z tych warunków nie gwarantuje zgodności indeksu z przywróconymi danymi.

<span id="5-rebuild-the-full-text-index"></span>

### 5. Odbuduj indeks pełnotekstowy

Klienci powinni pozostać odłączeni, a scheduler wyłączony. Po uruchomieniu wywołaj endpoint odbudowy, podając token administratora, jeśli uwierzytelnianie jest włączone (jeśli jest wyłączone, pomiń nagłówek):

``` bash
curl -fsS -X POST http://localhost:3030/v1/rebuild-tantivy \
  -H "Authorization: Bearer <admin_token>"
```

HTTP 202 oznacza rozpoczęcie pracy w tle, a nie jej powodzenie. Sprawdź w logu serwera komunikat `Tantivy rebuild: starting`, a następnie `Tantivy rebuild complete`. Jeśli pojawi się `Tantivy rebuild failed`, rozwiąż problem przed przywróceniem usługi. Odbudowa zastępuje indeks danymi z przywróconego RocksDB i może potrwać.

<span id="5-verify"></span>

### 6. Sprawdź

``` bash
curl -fsS http://localhost:3030/v1/status \
  -H "Authorization: Bearer <admin_token>"
```

Jeśli uwierzytelnianie jest wyłączone, pomiń nagłówek. Sprawdź oczekiwane dane checkpointu oraz przykładowe wyszukiwania, w tym rekordy, które powinny być obecne lub nieobecne. Sprawdź, czy `undecodable_chunks` ma wartość zero, i przeanalizuj błędy w logach; brak klucza lub uszkodzone rekordy mogą sprawić, że odbudowa będzie niepełna. Liczniki w `config_summary` są tylko wskazówką: `rocksdb_keys` jest wartością szacunkową, a zgodne liczby nie dowodzą, że indeks zawiera właściwe dokumenty. Po pomyślnej weryfikacji zatrzymaj serwer, przywróć poprzednią wartość `scheduler.enabled`, uruchom serwer ponownie, a następnie podłącz klientów.

<span id="data-retention-and-hard-purge"></span>

## Retencja danych i trwałe usuwanie

Usunięte wspomnienia są najpierw usuwane logicznie (oznaczane czasem `deleted_at`) i pozostają w magazynie przez okres umożliwiający odzyskanie. Po jego upływie proces trwałego usuwania usuwa je nieodwracalnie.

``` toml
[retention]
soft_delete_days = 30           # 30 dni na odzyskanie
hard_purge_interval_secs = 86400  # proces trwałego usuwania działa codziennie
```

**Trwałe usunięcie obejmuje:** dane fragmentów, embeddingi, metadane encji i relacji oraz odwołania grafu. Danych nie da się odzyskać z kopii utworzonych po trwałym usunięciu.

**Aby odzyskać logicznie usunięte wspomnienie przed trwałym usunięciem:** przywróć kopię wykonaną przed usunięciem.

<span id="recovery-metrics"></span>

## Parametry odzyskiwania

| Parametr | Wartość |
|----|----|
| RPO (docelowy punkt odzyskiwania) | 12 godzin (odstęp między kopiami) |
| RTO (docelowy czas odzyskiwania) | Zależy od ilości danych, szybkości dysku, odbudowy i weryfikacji; zmierz dla swojego wdrożenia. |

<span id="notes-for-container-platforms-eg-railway"></span>

## Uwagi dla platform kontenerowych (np. Railway)

- Trwały wolumen zamontowany w `/data` przetrwa ponowne wdrożenia
- Kopie znajdują się na tym samym wolumenie — jest to pojedynczy punkt awarii
- Aby przechowywać kopię poza serwerem, korzystaj z migawek wolumenu dostępnych na platformie lub okresowo pobieraj checkpoint
- Przed każdą destrukcyjną migracją sprawdź, czy istnieje aktualna kopia zapasowa
