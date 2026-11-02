<span id="loomem-quick-start"></span>

# Loomem — szybki start

Loomem daje Twoim agentom AI pamięć dostępną w kolejnych rozmowach. Może zapamiętywać fakty, decyzje i preferencje, które im przekażesz.

<span id="setup-about-2-minutes-local"></span>

## Konfiguracja lokalna

<span id="1-install-and-start-the-server"></span>

### 1. Zainstaluj i uruchom serwer

Zainstaluj gotowe pliki wykonywalne (bez `sudo`, w katalogu `~/.loomem`) i dodaj `~/.loomem/bin` do zmiennej `PATH`. Szczegółowe kroki znajdziesz w [instrukcji instalacji](installation.md). Przed pierwszym uruchomieniem pobierz lokalny model embeddingów zgodnie z tą instrukcją. Domyślna konfiguracja wymaga również klucza LLM `OPENAI_API_KEY`; alternatywą jest tryb o ograniczonej funkcjonalności `LOOMEM_ALLOW_KEYLESS=1`. Następnie uruchom serwer:

``` bash
cd ~/.loomem && loomem-server
# nasłuchuje na http://localhost:3030 — uwierzytelnianie jest domyślnie wyłączone lokalnie
```

<span id="2-connect-claude-code"></span>

### 2. Połącz Claude Code

``` bash
claude mcp add --transport http loomem http://localhost:3030/mcp
```

<span id="2b-connect-claude-desktop"></span>

### 2b. Możesz też połączyć aplikację Claude na komputerze (oraz Cowork)

**Aplikacja Claude na komputerze** łączy się z lokalnymi serwerami przez **stdio**, a nie HTTP. Pole własnego konektora przyjmuje tylko adresy `https://`, więc nie można tam wpisać `http://localhost`. Dodaj most w pliku `claude_desktop_config.json` (na macOS: `~/Library/Application Support/Claude/claude_desktop_config.json`), a następnie całkowicie uruchom Claude ponownie. Most natywny nie wymaga Node.js (Loomem ≥ v0.2.1 — użyj bezwzględnej ścieżki do `loomem-cli`):

``` json
{
  "mcpServers": {
    "loomem": {
      "command": "/Users/you/.loomem/bin/loomem-cli",
      "args": ["mcp-stdio", "--url", "http://127.0.0.1:3030"]
    }
  }
}
```

Alternatywnie, z Node.js możesz użyć uniwersalnego rozwiązania [`mcp-remote`](https://www.npmjs.com/package/mcp-remote):

``` json
{
  "mcpServers": {
    "loomem": {
      "command": "npx",
      "args": ["-y", "mcp-remote", "http://127.0.0.1:3030/mcp", "--allow-http"]
    }
  }
}
```

<span id="3-try-it"></span>

### 3. Wypróbuj

W trybie `LOOMEM_ALLOW_KEYLESS=1` narzędzie `memory_ingest` bez klucza nie wyodrębnia faktów i zwraca błąd. Może jednak zapisać tekst źródłowy przed próbą ekstrakcji. Aby przetestować ekstrakcję w poniższym przykładzie, skonfiguruj klucz LLM.

Powiedz Claude:

> „Zapamiętaj, że we wszystkich narzędziach preferuję tryb ciemny.”

Następnie rozpocznij nową rozmowę i zapytaj:

> „Co wiesz o moich preferencjach?”

Claude może odnaleźć odpowiedź w pamięci Loomem.

<span id="connecting-claudeai-or-chatgpt-remote"></span>

## Połączenie z claude.ai lub ChatGPT przez internet

Te usługi łączą się ze **zdalnymi** serwerami MCP przez HTTPS. To osobny, dłuższy etap: udostępnij instancję za TLS, ustaw `SERVER_ORIGIN`, a następnie dodaj konektor wskazujący na `https://your-domain/mcp`. Dynamiczna rejestracja klienta OAuth jest obsługiwana. Pełny opis TLS, reverse proxy i uwierzytelniania znajdziesz w [instrukcji wdrożenia](deployment.md).

<span id="how-it-works"></span>

## Jak to działa

- Claude może zapisywać udostępniane fakty w pamięci
- Claude może przeszukiwać pamięć przed odpowiedzią na pytania o Ciebie
- Pamięć pozostaje dostępna między rozmowami i sesjami oraz na urządzeniach mających dostęp do serwera

<span id="privacy"></span>

## Prywatność

Pamięć jest przechowywana na Twoim serwerze. Zależnie od konfiguracji funkcje oparte na LLM mogą korzystać z zewnętrznego dostawcy. Więcej w [opisie bezpieczeństwa](SECURITY.md).

<span id="more"></span>

## Więcej informacji

- [Podręcznik użytkownika](https://github.com/vvooki-sys/loomem/blob/main/docs/pl/user-guide.md) — codzienna obsługa
- [Opis narzędzi MCP](mcp-tools.md) — operacje na pamięci
