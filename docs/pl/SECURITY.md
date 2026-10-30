<span id="security-overview"></span>

# Przegląd bezpieczeństwa

Ostatnia aktualizacja: 2026-10-21

Ten dokument opisuje model bezpieczeństwa Loomem: uwierzytelnianie, szyfrowanie danych w spoczynku i podczas transmisji, obsługę danych osobowych oraz logowanie. Brakujące mechanizmy ochrony są wskazane w części [Znane ograniczenia](#known-limitations).

------------------------------------------------------------------------

<span id="threat-model-summary"></span>

## Podsumowanie modelu zagrożeń

**Przy włączonym szyfrowaniu danych w spoczynku (poniżej) Loomem chroni przed:**

- **Ujawnieniem treści pamięci wskutek wycieku migawki wolumenu lub kopii całego dysku.** Treść fragmentów i wpisy grafu encji są szyfrowane na poziomie rekordów w RocksDB, zanim trafią do warstwy magazynu.
- **Wyciekiem plików kopii zapasowej RocksDB** (np. checkpointu skopiowanego poza host). Rekordy fragmentów i grafu encji w checkpoincie są zaszyfrowane.
- **Wycofaniem lub ponownym przydzieleniem nośników danych.** Szyfrowanie w warstwie aplikacji zapewnia ochronę niezależną od szyfrowania dysku przez hosta.

**Loomem NIE chroni przed:**

- **Przejęciem działającego procesu.** Jeśli napastnik uzyska dostęp do pamięci procesu `loomem-server` (środowiska lub zrzutów pamięci), może odczytać klucz główny i odszyfrować klucze DEK strumieni.
- **Dostępem operatora.** Osoba kontrolująca środowisko hosta dysponuje kluczem głównym i może odszyfrować całą przechowywaną treść. Loomem nie działa w modelu zero-knowledge, nie ukrywa danych przed operatorem i nie stosuje szyfrowania end-to-end: dane jawne przekraczają granicę zaufania w procesie serwera, który odszyfrowuje je, aby udzielić odpowiedzi.
- **Odczytem przez dostawcę LLM.** Treść pamięci jest wysyłana jawnym tekstem do skonfigurowanego dostawcy LLM (np. OpenAI) na potrzeby generowania embeddingów, konsolidacji i wykrywania sprzeczności. Zapoznaj się z polityką retencji danych dostawcy.
- **Ujawnieniem indeksu pełnotekstowego i embeddingów.** Indeks Tantivy wymaga jawnych tokenów do wyszukiwania BM25, a wektory embeddingów są przechowywane bez szyfrowania (są potrzebne do wyszukiwania kosinusowego i według opublikowanych badań częściowo odwracalne). Zobacz [Znane ograniczenia](#known-limitations).

------------------------------------------------------------------------

<span id="authentication"></span>

## Uwierzytelnianie

Loomem korzysta z jednego klucza API:

- Ustaw zmienną środowiskową wskazaną przez `server.auth_token_env` w `config.toml` (domyślnie `LOOMEM_AUTH_TOKEN`).
- Wszystkie żądania z wyjątkiem `GET /health` muszą zawierać `Authorization: Bearer <key>`.
- Jeśli nie skonfigurowano klucza, serwer pracuje w **lokalnym trybie bez uwierzytelniania**: każde żądanie jest akceptowane z uprawnieniami administratora. Używaj go tylko do lokalnego rozwoju na zaufanej maszynie; nigdy nie udostępniaj nieuwierzytelnionej instancji w sieci.
- **Zabezpieczenie uruchamiania:** jeśli `server.host` nie jest adresem loopback i nie skonfigurowano klucza, serwer **odmawia uruchomienia**, zamiast udostępnić nieuwierzytelnione API administratora. Dotyczy to również obrazu Docker nasłuchującego na `0.0.0.0`. Ustaw `LOOMEM_ALLOW_UNAUTH=1`, aby świadomie wyłączyć tę ochronę, np. za reverse proxy z uwierzytelnianiem.

Klienci MCP, którzy nie mogą wysyłać własnych nagłówków, mogą użyć wbudowanego przepływu OAuth 2.0 (`/oauth/register`, `/oauth/authorize`, `/oauth/token`): użytkownik wpisuje klucz API raz podczas autoryzacji, a uzyskany token dostępu jest równoważny kluczowi.

Zalecenia:

- Wygeneruj długi losowy klucz (co najmniej 64 znaki szesnastkowe).
- Przechowuj go wyłącznie w zmiennych środowiskowych — nigdy w `config.toml` ani w repozytorium.
- W razie podejrzenia wycieku zmień klucz, ustawiając nową wartość zmiennej i ponownie uruchamiając serwer.

Ochrona przed nadużyciami: opcjonalne limity żądań dla poszczególnych strumieni obejmują najczęściej używane ścieżki (`[rate_limit]` w `config.toml`, domyślnie wyłączone; oficjalny obraz Docker je włącza). Po przekroczeniu limitu zwracany jest HTTP `429` z nagłówkiem `Retry-After` dla żądań `/v1`, a MCP zwraca błąd narzędzia.

------------------------------------------------------------------------

<span id="data-at-rest"></span>

## Dane w spoczynku

<span id="without-encryption-enabled-default"></span>

### Bez włączonego szyfrowania (domyślnie)

| Obszar | Jawny tekst? | Uwagi |
|----|----|----|
| Fragmenty RocksDB (`chunk:L*:{id}`) | tak | Skompresowane (LZ4/Snappy), ale nieszyfrowane w warstwie aplikacji. |
| Rekordy encji, relacji i grafu RocksDB | tak | Jak wyżej. |
| Indeks pełnotekstowy Tantivy | tak | Jawny tekst jest wymagany do wyszukiwania BM25. |
| Wektory embeddingów (rodzina kolumn `embeddings`) | tak | Wymagane do wyszukiwania kosinusowego. |
| Dziennik WAL / intent log | nie dotyczy | Zawiera tylko typ operacji i identyfikator fragmentu — bez treści. |

Jeżeli dostawca hostingu szyfruje wolumen w warstwie hosta, otrzymujesz ochronę danych w spoczynku typową dla zarządzanej platformy, ale checkpoint bazy, który wyciekł, pozostaje czytelny.

<span id="with-encryption-enabled"></span>

### Z włączonym szyfrowaniem

Ustaw `LOOMEM_AT_REST_MASTER_KEY` (32 bajty zakodowane w base64), aby włączyć szyfrowanie kopertowe w warstwie aplikacji:

- **Algorytm:** AES-256-GCM.
- **Hierarchia kluczy:**

`master_key (zmienna środowiskowa LOOMEM_AT_REST_MASTER_KEY) └─ zabezpiecza → klucz szyfrowania danych strumienia (DEK) └─ szyfruje → treść fragmentu, nazwy encji, dane relacji`

Klucze DEK strumieni są generowane przy pierwszym szyfrowanym zapisie i przechowywane jako zabezpieczone bloby w osobnej rodzinie kolumn RocksDB. Rotacja klucza głównego (ponowne zabezpieczenie DEK nowym kluczem głównym) jest **planowana, ale jeszcze niezaimplementowana**. Obecnie zmiana `LOOMEM_AT_REST_MASTER_KEY` *nie* zabezpiecza kluczy ponownie, a danych zapisanych starym kluczem nie można odszyfrować.

- **Szyfrowane:** zawartość i metadane fragmentów, wartości encji i relacji, nazwy i aliasy encji grafu oraz rekordy wkładu do grafu przypisane do wspomnień (`graph:labels:` i `graph:edge_labels:`), gdy włączono szyfrowanie danych w spoczynku.
- **Nieszyfrowane (celowo):** wektory embeddingów i indeks Tantivy (wymagane do wyszukiwania) oraz metadane routingu (identyfikator i poziom fragmentu, identyfikator strumienia, znaczniki czasu, łańcuch zastąpień) potrzebne do filtrowania, retencji i wygaszania.
- **Starsze dane:** jawne rekordy fragmentów oraz wartości encji i relacji zapisane przed włączeniem szyfrowania są rozpoznawane po braku specjalnego prefiksu i pozostają czytelne. Rekordy wkładu do grafu podlegają ostrzejszej regule: po włączeniu szyfrowania jawne rekordy są blokowane i pozostają niedostępne do zakończenia szyfrowania wstecznego. `POST /v1/admin/backfill/encrypt-at-rest` przetwarza istniejące rekordy i szyfruje je w sposób idempotentny; postęp sprawdzisz przez `GET /v1/admin/backfill/encrypt-at-rest/status`. Operacja obejmuje również rekordy wkładu do grafu. Podczas przejścia z jawnej bazy na szyfrowanie uruchom po niej osobną procedurę `POST /v1/admin/rekey-name-index`; szyfrowanie wartości nie zmienia kluczy indeksów wyszukiwania nazw i aliasów.
- **Endpoint stanu:** `GET /v1/encryption/status` podaje, czy szyfrowanie jest aktywne, oraz odcisk klucza głównego (jednokierunkowy skrót, który można bezpiecznie zapisać obok kopii zapasowych).
- **Wymuszanie szyfrowania:** ustaw `LOOMEM_AT_REST_EXPECT_ENABLED=1`, aby serwer odmówił uruchomienia bez klucza głównego. Zapobiega to przypadkowemu uruchomieniu zaszyfrowanej bazy w trybie jawnym. Oficjalny obraz Docker ustawia to domyślnie (aby zrezygnować, ustaw `LOOMEM_AT_REST_EXPECT_ENABLED=0`); poza Dockerem uruchomienie bez klucza wyświetla wyraźne ostrzeżenie.

Wkłady do grafu są powiązane z właścicielem encji lub krawędzi, identyfikatorem wspomnienia, strumieniem i generacją treści. Wyszukiwanie i wyświetlanie wymagają aktywnych wspomnień potwierdzających. Starsze aliasy bez jawnych dowodów dla poszczególnych wspomnień pozostają ukryte do ponownej ekstrakcji. Przy uruchomieniu usuwane są nieaktualne surowe znaczniki JSON zakończenia ekstrakcji, bez usuwania wspomnień kanonicznych i wywoływania modelu. Fizyczne usunięcie usuwa powiązane rekordy wkładu do grafu; usunięcie logiczne nadal podlega istniejącej polityce odzyskiwania i usuwania, więc nie oznacza natychmiastowego fizycznego wymazania.

**Przechowuj kopię klucza głównego oddzielnie od danych.** Zaszyfrowanego checkpointu nie można odzyskać bez pasującego klucza głównego. Zobacz [Kopia zapasowa i przywracanie](backup-and-restore.md).

<span id="source-fragments-and-the-envelope"></span>

### Fragmenty źródłowe a koperta szyfrowania

Opcjonalna funkcja fragmentów źródłowych ([`[fragments]`](configuration.md#fragments), domyślnie `enabled = false`) zapisuje pochodzenie każdego fragmentu — identyfikator fragmentu źródłowego, zakres bajtów w źródle, indeks tury, rolę autora i datę rozmowy — jako pole najwyższego poziomu fragmentu, **poza** szyfrowanym ładunkiem. Koperta obejmuje tylko `(content, metadata, extraction_meta)`; pochodzenie fragmentu jest zapisywane obok jawnych pól routingu. W rezultacie przy włączonym szyfrowaniu i fragmentach dzień rozmowy, struktura tur i rola autora każdego fragmentu są czytelne z surowej bazy, choć ta sama data wewnątrz szyfrowanych metadanych fragmentu nie jest. Tekst fragmentu jest szyfrowany tak samo jak treść każdego innego fragmentu.

**Zasada operacyjna:** nie włączaj `[fragments]` w instancji korzystającej z szyfrowania danych w spoczynku, dopóki pochodzenie fragmentu nie zostanie przeniesione do koperty (planowane jako osobna zmiana wymagająca migracji). W samodzielnie hostowanych instancjach bez tego szyfrowania nie ma to znaczenia — wszystkie pola i tak są jawne.

------------------------------------------------------------------------

<span id="data-in-transit"></span>

## Dane w transmisji

- Loomem obsługuje zwykły HTTP; uruchamiaj go za reverse proxy kończącym TLS lub na platformie zapewniającej HTTPS.
- Wychodzące połączenia HTTPS z dostawcą LLM korzystają z `reqwest` i `rustls`.
- Transport MCP to JSON-RPC przez HTTPS na `/mcp`, uwierzytelniany tokenem Bearer. Poza TLS nie ma dodatkowej własnej warstwy szyfrowania.

------------------------------------------------------------------------

<span id="pii-filtering"></span>

## Filtrowanie danych osobowych

Gdy `[pii]` jest włączone w `config.toml`, numery telefonów, adresy e-mail, krajowe numery identyfikacyjne i terminy z listy blokowanej są ukrywane **przed każdym wywołaniem API LLM** (konsolidacja, ekstrakcja, dream). Zastępowane są tokenami `[PHONE]`, `[EMAIL]`, `[ID]` i `[REDACTED]`. Zobacz [Konfiguracja](configuration.md#pii).

Sanityzacja podczas ingestu (usuwanie HTML, wykrywanie wstrzykiwania instrukcji) zapisuje podejrzane dane w logach, ale ich nie blokuje — zobacz [Architektura](architecture.md).

------------------------------------------------------------------------

<span id="secrets-management"></span>

## Zarządzanie sekretami

Zmienne środowiskowe, które mogą zawierać sekrety:

| Zmienna | Przeznaczenie |
|----|----|
| `OPENAI_API_KEY` (lub zmienna wskazana przez `llm.api_key_env`) | API LLM (embeddingi i ekstrakcja) |
| `LOOMEM_AUTH_TOKEN` (lub zmienna wskazana przez `server.auth_token_env`) | Klucz Bearer API |
| `LOOMEM_AT_REST_MASTER_KEY` | Klucz główny szyfrowania kopertowego |
| `TELEGRAM_BOT_TOKEN`, `LOOMEM_TELEGRAM_CHAT_ID` | Opcjonalny webhook alertów kosztowych |

Przechowuj je w magazynie sekretów platformy lub w konfiguracji środowiska — nigdy w `config.toml`, repozytorium ani artefaktach kompilacji.

------------------------------------------------------------------------

<span id="logging"></span>

## Logowanie

Loomem zapisuje logi na **stdout** (12-factor); nie wysyła ich, nie przechowuje ani samodzielnie nie generuje na ich podstawie alertów.

- W środowisku produkcyjnym ustaw `LOOMEM_LOG_FORMAT=json`, aby uzyskać ustrukturyzowane wyjście JSON. Domyślny format jest zwięzły i czytelny dla człowieka podczas lokalnego rozwoju.
- Zdarzenia związane z bezpieczeństwem (błędy uwierzytelniania, usunięcia, działania administracyjne) są oznaczane `target: "audit"`; system zbierający logi może filtrować je według tego oznaczenia.
- W środowisku produkcyjnym skieruj stdout do systemu zbierania logów z polityką retencji. Rozważ alerty na wzrost liczby błędów `target=audit` i powtarzające się błędy uwierzytelniania.

------------------------------------------------------------------------

<span id="known-limitations"></span>

## Znane ograniczenia

1.  **Przejęcie działającego procesu ujawnia klucz główny.** Dostęp do pamięci procesu pozwala odszyfrować wszystko. Ograniczenie tego zagrożenia wymagałoby wdrożenia typu TEE, którego Loomem obecnie nie obsługuje.
2.  **Indeks Tantivy zawiera jawny tekst.** Jest on wymagany do pełnotekstowego wyszukiwania BM25.
3.  **Wektory embeddingów są jawne.** Są wymagane do wyszukiwania wektorowego i częściowo odwracalne.
4.  **Dostawca LLM widzi jawny tekst** podczas ingestu i konsolidacji.
5.  **Szyfrowanie kopii zapasowych jest dziedziczone, a nie dodawane.** Checkpointy RocksDB korzystają z opisanego wyżej szyfrowania rekordów (gdy jest włączone) oraz szyfrowania wolumenu zapewnianego przez hosta; Loomem nie dodaje osobnej warstwy szyfrowania kopii.
6.  **Nie ma jeszcze narzędzi do rotacji klucza głównego.** Ponowne zabezpieczenie DEK nowym kluczem jest planowane; obecnie zmiana klucza głównego uniemożliwia odszyfrowanie wcześniej zapisanych danych.

Loomem nie posiada potwierdzeń zgodności SOC 2, HIPAA ani ISO 27001. Jeżeli są potrzebne, dotyczą sposobu wdrożenia i organizacji, a nie samego oprogramowania.

------------------------------------------------------------------------

<span id="reporting-security-issues"></span>

## Zgłaszanie problemów bezpieczeństwa

Jeśli odkryjesz problem bezpieczeństwa w Loomem, zgłoś go prywatnie opiekunom projektu (na przykład przez funkcję prywatnego zgłaszania podatności w serwisie hostującym kod), zamiast publikować publiczne zgłoszenie z instrukcją odtworzenia.
