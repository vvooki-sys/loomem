# Podręcznik użytkownika

Ten przewodnik jest przeznaczony dla osób, które chcą połączyć Claude z instancją Loomem i korzystać z trwałej pamięci.

---

## Czym jest Loomem?

Loomem udostępnia Claude pamięć między rozmowami. Claude może zapisywać i przywoływać informacje o Tobie, Waszych rozmowach i podjętych decyzjach. Pamięć jest dostępna z urządzeń, które mogą połączyć się z Twoją instancją.

---

## Konfiguracja

Od osoby zarządzającej Twoim serwerem Loomem potrzebujesz adresu serwera, a jeśli włączono uwierzytelnianie, także klucza API. Możesz też samodzielnie uruchomić serwer — zobacz [instrukcję wdrożenia](guide/deployment.html).

- **Adres serwera** — na przykład `https://your-server.app/mcp`.
- **Klucz API** — skonfigurowany na serwerze token Bearer (na przykład `loom_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx`). Jest potrzebny, jeśli serwer wymaga uwierzytelniania.

### Claude.ai (przeglądarka)

1. Otwórz **Settings** → **Customize** → **Connectors**.
2. Wybierz **Add connector**.
3. Wklej adres serwera.
4. Gdy pojawi się prośba o autoryzację, podaj dane wymagane przez konfigurację serwera.
5. Gotowe.

### Claude Desktop (macOS / Windows)

W aplikacji Claude Desktop otwórz **Settings** → **Developer** → **Edit Config**, zmodyfikuj `claude_desktop_config.json`, a następnie **całkowicie uruchom ponownie** Claude Desktop. Właściwy fragment konfiguracji zależy od miejsca, w którym działa Loomem.

**Loomem działa lokalnie na tym komputerze** (typowy przypadek). Aplikacja komputerowa łączy się z lokalnymi serwerami przez **stdio**, a nie HTTP, zaś pole własnego konektora przyjmuje wyłącznie adresy `https://`. Nie można więc wskazać w nim `http://localhost`. Połącz lokalny punkt HTTP ze stdio za pomocą mostu. Są na to dwa sposoby:

*Zalecany — most natywny, bez Node.js* (Loomem ≥ v0.2.1; wbudowany w `loomem-cli`):

```json
{
  "mcpServers": {
    "loomem": {
      "command": "/Users/you/.loomem/bin/loomem-cli",
      "args": ["mcp-stdio", "--url", "http://127.0.0.1:3030"]
    }
  }
}
```

Użyj **bezwzględnej** ścieżki do `loomem-cli` — aplikacja komputerowa nie dziedziczy zmiennej `PATH` powłoki. Jeśli uwierzytelnianie jest włączone, dodaj `"--token", "loom_your_api_key_here"` do `args` albo ustaw `LOOMEM_AUTH_TOKEN` w bloku `"env"`.

*Alternatywa — `mcp-remote`* (działa z dowolną wersją, wymaga Node 18+):

```json
{
  "mcpServers": {
    "loomem": {
      "command": "npx",
      "args": ["-y", "mcp-remote", "http://127.0.0.1:3030/mcp", "--allow-http"]
    }
  }
}
```

Flaga `--allow-http` jest wymagana dla lokalnego punktu dostępnego przez zwykły HTTP. W przypadku uwierzytelniania użyj `"args": ["-y", "mcp-remote", "http://127.0.0.1:3030/mcp", "--allow-http", "--header", "Authorization: Bearer ${LOOMEM_AUTH_TOKEN}"]` oraz `"env": { "LOOMEM_AUTH_TOKEN": "loom_your_api_key_here" }`.

**Loomem jest hostowany gdzie indziej i dostępny przez HTTPS.** Wskaż zdalny adres bezpośrednio w konektorze — most nie jest potrzebny:

```json
{
  "mcpServers": {
    "loomem": {
      "url": "https://your-server.app/mcp",
      "headers": {
        "Authorization": "Bearer loom_your_api_key_here"
      }
    }
  }
}
```

### Claude Code (CLI / VS Code)

Dodaj do pliku `.mcp.json` projektu albo do globalnej konfiguracji MCP:

```json
{
  "mcpServers": {
    "loomem": {
      "url": "https://your-server.app/mcp",
      "headers": {
        "Authorization": "Bearer loom_your_api_key_here"
      }
    }
  }
}
```

---

## Jak korzystać

Po połączeniu Claude może korzystać z pamięci podczas rozmowy. Nie musisz uczyć się poleceń — rozmawiaj naturalnie. To, czy i kiedy klient wywoła narzędzia, zależy od jego zachowania i konfiguracji. Funkcje ekstrakcji zależą też od konfiguracji LLM na serwerze.

### Claude może zapamiętywać informacje

```text
Ty:     „Jestem analitykiem danych i pracuję w Acme Corp”.
Claude: [zapisuje: użytkownik jest analitykiem danych w Acme Corp]

        ... kolejna rozmowa ...

Ty:     „Pomożesz mi z tym zbiorem danych?”
Claude: [przywołuje: użytkownik jest analitykiem danych]
        „Jasne! Ponieważ zajmujesz się analizą danych, skupię się na...”
```

### Poproś Claude o zapamiętanie

```text
Ty:     „Zapamiętaj, że we wszystkich narzędziach preferuję tryb ciemny”.
Claude: [zapisuje preferencję]
```

### Poproś Claude o przypomnienie

```text
Ty:     „Co wiesz o moich preferencjach?”
Claude: [przeszukuje pamięć i zwraca odpowiednie fakty]
```

### Poproś Claude o zapomnienie

```text
Ty:     „Zapomnij, że pracuję w Acme Corp”.
Claude: [usuwa odpowiedni wpis pamięci]
```

Usunięcie pojedynczego faktu pochodzącego ze źródła nie usuwa automatycznie źródła ani pozostałych faktów z nim powiązanych. Aby usunąć również źródło rozmowy i powiązane z nim wspomnienia z wyników wyszukiwania, należy usunąć jego identyfikator źródła.

---

## Co trafia do pamięci?

Claude może zapisywać:

- **Preferencje** — narzędzia, języki, styl kodowania, nawyki pracy.
- **Decyzje** — wybory technologii i kierunki rozwoju projektów.
- **Fakty o Tobie** — rola, wiedza i projekty.
- **Stan projektów** — terminy, cele i aktualny status.

Wskazówki dla klienta zalecają zapisywanie konkretnych, przydatnych informacji, a pomijanie pogawędek, powitań, spekulacji i krótkotrwałych aktualizacji statusu jako osobnych faktów. `memory_ingest` może zachować również oczyszczone źródło rozmowy wraz z wyodrębnionymi faktami. Nie zakładaj, że żaden fragment rozmowy nie jest przechowywany. Nie udostępniaj sekretów w rozmowach przeznaczonych do zapisu w pamięci.

---

## Jak działa pamięć — w uproszczeniu

### Warstwy

Pamięć jest uporządkowana w warstwach:

1. **Świeże wspomnienia** — najnowsze, szczegółowe informacje.
2. **Wspomnienia skompresowane** — powiązane fakty połączone w bardziej zwięzłą postać.
3. **Wiedza podstawowa** — trwałe informacje, takie jak tożsamość i najważniejsze preferencje.

Świeże wpisy mogą z czasem wygasać. Ważne informacje są konsolidowane, a często przywoływane wpisy mogą utrzymywać się dłużej.

### Konsolidacja podczas „snu”

Loomem może uruchamiać proces „snu”, który:

- grupuje powiązane wspomnienia,
- łączy podobne informacje,
- rozwiązuje sprzeczności, na przykład po zmianie preferowanego IDE,
- kompresuje obszerne wspomnienia do zwięzłych faktów.

Proces można wywołać przez `memory_dream` albo uruchamiać automatycznie po osiągnięciu skonfigurowanego progu. Wymaga dostępności odpowiednich funkcji opartych na LLM.

### Obsługa sprzeczności

Jeśli przekażesz Claude informację sprzeczną z wcześniejszym wpisem:

```text
Marzec:   „Używam VSCode”.
Kwiecień: „Przeszedłem na Cursor”.
```

Loomem może utworzyć łańcuch wersji: poprzedni fakt zostaje oznaczony jako zastąpiony, a nowy staje się aktualny. Claude może wtedy oprzeć odpowiedź na nowszej wersji.

---

## Prywatność

- **Jedna osoba z założenia** — Loomem jest przeznaczony dla jednego użytkownika. Token Bearer może chronić dostęp do całej instancji; w lokalnej konfiguracji uwierzytelnianie jest domyślnie wyłączone, więc zabezpiecz serwer odpowiednio do sposobu wdrożenia.
- **Ochrona danych osobowych** — domyślna konfiguracja włącza redakcję numerów telefonów, adresów e-mail i identyfikatorów. Skuteczność zależy od konfiguracji i rozpoznania danych, więc nie traktuj filtra jako gwarancji wykrycia wszystkich wrażliwych informacji.
- **Twój serwer, Twoje dane** — pamięć jest przechowywana na serwerze, który uruchamiasz. Funkcje korzystające z LLM mogą przekazywać dane do skonfigurowanego dostawcy.

---

## Wskazówki

1. **Wyrażaj się konkretnie** — „Wolę Tailwind CSS od styled-components ze względu na podejście utility-first” jest lepsze niż „Lubię Tailwind”.
2. **Dziel się kontekstem** — na początku opowiedz Claude o swojej roli, projektach i celach.
3. **Poprawiaj błędy** — jeśli Claude pamięta coś nieprawidłowo, poproś o aktualizację.
4. **Korzystaj w kolejnych sesjach** — dzięki zapisanym informacjom Claude może lepiej rozumieć Twój kontekst.
5. **Nie przejmuj się duplikatami** — Loomem może automatycznie deduplikować podobne fakty.

---

## Rozwiązywanie problemów

### „Claude chyba niczego nie pamięta”

- Sprawdź, czy konektor jest aktywny w **Settings** → **Customize** → **Connectors**.
- Zapytaj: „Co o mnie wiesz?” i sprawdź, czy Claude korzysta z narzędzi pamięci.
- Jeśli serwer wymaga klucza API, sprawdź jego poprawność.
- Sprawdź konfigurację LLM: w trybie bez klucza `memory_store` i wyszukiwanie działają, ale funkcje ekstrakcji korzystające z LLM są niedostępne.

### „Claude pamięta nieprawidłowe informacje”

- Zapytaj: „Jakie wspomnienia masz na temat [temat]?”
- Powiedz: „To nieaktualne, popraw proszę: [nowy fakt]”.
- Albo: „Usuń wspomnienie o [temat]”.

### „Pamięć działa wolno”

- Pierwsze wyszukiwanie może trwać dłużej, zanim pamięć podręczna zostanie zapełniona.
- Kolejne wyszukiwania mogą korzystać z pamięci podręcznej, jeśli konfiguracja na to pozwala.
- Czas odpowiedzi zależy od środowiska i konfiguracji.

---

## Masz pytania?

W przypadku problemów z połączeniem lub kluczem API sprawdź logi serwera i [instrukcję wdrożenia](guide/deployment.html) albo zapytaj osobę zarządzającą serwerem Loomem.
