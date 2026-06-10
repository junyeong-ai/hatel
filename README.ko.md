# hatel

[![CI](https://github.com/junyeong-ai/hatel/actions/workflows/ci.yml/badge.svg)](https://github.com/junyeong-ai/hatel/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#)

> **[English](README.md)** | **한국어**

Claude Code를 위한 로컬·무인프라 텔레메트리 수집기입니다. Claude Code가 이미 생성하는 두 개의
상호 보완적인 신호 계층 — 네이티브 OpenTelemetry와 라이프사이클 훅 — 을 프로젝트별·세션별·
서브에이전트별 뷰로 결합합니다. 호스팅할 대시보드가 없고, 기본적으로 데이터는 머신을 떠나지
않습니다(옵트인 [export](#다른-컬렉터로-전달export)로 enriched 스트림을 하류 컬렉터에 tee 가능).

- **네이티브 OTel** (push) 은 머신 신호를 운반합니다: 토큰, 비용, 활성 시간, 라인 수,
  `agent.name`을 통한 서브에이전트별 귀속, 그리고 도구별 소요 시간·성공 여부(`tool_result`
  이벤트). 와이어 상에 프로젝트 정보가 없으므로 세션 인덱스를 통해 프로젝트에 결합됩니다.
- **훅** (event) 은 프로젝트 컨텍스트(`cwd`)와 OTel이 표현할 수 없는 도메인 이벤트를 운반합니다:
  프롬프트 크기, 메모리 로드, 서브에이전트 종료, 컴팩션 — 그리고 플러그인이 정의하는
  무엇이든.

두 개의 바이너리:

| 바이너리 | 역할 |
|---|---|
| `hatel-hook` | `settings.json` 훅에 연결됨. stdin으로 이벤트 하나를 읽어 등록된 바인딩으로 매핑하고 일치하는 것을 기록한 뒤 종료. 비동기 런타임 없음 — 콜드 스타트 ~3 ms. |
| `hatel` | 리시버(`serve`), 리포트, `init`, `service`, `doctor`, `kinds`. |

## 설치

한 줄 — 사전 빌드된 바이너리(리시버 + 훅)를 내려받아 SHA-256을 검증하고 Claude Code 스킬을
설치합니다. Rust 툴체인이 필요 없습니다:

```sh
curl -fsSL https://raw.githubusercontent.com/junyeong-ai/hatel/main/scripts/install.sh | bash
```

같은 단계에서 Claude Code 와이어링까지 하려면 `-s -- --wire`를 덧붙이고(`| bash -s -- --wire`),
특정 릴리스를 고정하려면 `HATEL_VERSION=0.1.0`을 사용하세요. 나중에 모두 제거하려면
대응되는 `scripts/uninstall.sh`를 실행합니다.

클론한 저장소에서는 같은 스크립트가 플랫폼용 사전 빌드 릴리스가 없을 때 소스에서 두 바이너리를
빌드합니다(또는 `--source`로 강제):

```sh
git clone https://github.com/junyeong-ai/hatel && cd hatel
./scripts/install.sh            # 사전 빌드가 있으면 사용, 없으면 소스 빌드
```

또는 cargo로 git에서 바로 설치:

```sh
cargo install --git https://github.com/junyeong-ai/hatel hatel-cli   # 리시버
cargo install --git https://github.com/junyeong-ai/hatel hatel-hook  # 훅
```

## Claude Code에 연결하기

`hatel init`이 이 작업을 대신 해줍니다 — 텔레메트리 `env`와 라이프사이클 훅을
`settings.json`에 멱등적·비파괴적으로 병합합니다(기존 훅을 건드리지 않고 우리 훅을 추가하며,
사내 컬렉터로 재지정해 둔 엔드포인트는 절대 덮어쓰지 않습니다):

```sh
hatel init                 # ~/.claude/settings.json (모든 프로젝트)
hatel init --scope local   # .claude/settings.local.json (이 저장소, 개발자별)
hatel doctor               # 와이어링을 검증하고 빠진 부분을 설명
hatel init --remove        # 깔끔하게 되돌리기 (네이티브 텔레메트리 env는 유지)
```

Claude Code 자체의 텔레메트리 설정은 반드시 `settings.json`의 `env`에 있어야 합니다 — 이것이
Claude Code가 세션 시작 시 읽는 유일한 채널이며, 그 `OTEL_*` 변수들은 의도적으로 훅 서브프로세스에
**전달되지 않습니다**. 두 계층이 분리된 이유가 바로 이것입니다. 조직 단위라면, 동등한 블록을
(`hatel init --print`이 출력) managed settings에 붙여넣으세요. 전체 형태는 다음과 같습니다:

```jsonc
{
  "env": {
    "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
    "OTEL_METRICS_EXPORTER": "otlp",
    "OTEL_LOGS_EXPORTER": "otlp",
    "OTEL_EXPORTER_OTLP_PROTOCOL": "http/json",
    "OTEL_EXPORTER_OTLP_ENDPOINT": "http://127.0.0.1:4318"
  },
  "hooks": {
    "SessionStart":      [{ "hooks": [{ "type": "command", "command": "hatel-hook" }] }],
    "UserPromptSubmit":  [{ "hooks": [{ "type": "command", "command": "hatel-hook" }] }],
    "SubagentStop":      [{ "hooks": [{ "type": "command", "command": "hatel-hook" }] }],
    "InstructionsLoaded":[{ "hooks": [{ "type": "command", "command": "hatel-hook" }] }],
    "PreCompact":        [{ "hooks": [{ "type": "command", "command": "hatel-hook" }] }]
  }
}
```

`init`은 로드된 Kind가 소비하는 이벤트만 연결합니다(`SessionStart`는 세션→프로젝트 인덱스를 위해
항상 연결). 도구 호출은 여기에 없습니다 — 소요 시간·성공 여부는 훅이 아니라 네이티브 `tool_result`
이벤트에서 옵니다. 다른 라이프사이클 이벤트(예: `PostToolUse`)를 바인딩하는 플러그인은 다음
`hatel init`에서 해당 이벤트가 연결됩니다.

위 `command`는 가독성을 위해 짧은 이름으로 표기했지만, `hatel init`(및 `--print`)은 `hatel` 옆의
`hatel-hook`에 대한 **절대 경로**를 기록합니다 — Claude Code가 `PATH`에 의존하지 않고 실행할 수
있도록. `http/json`은 필수입니다: 이 리시버는 JSON OTLP 인코딩을 디코드하므로 protobuf 의존성이
필요 없습니다. 블록을 **user** 레벨에 두면 하나의 컬렉터가 모든 프로젝트로부터 데이터를 받습니다.
리시버는 기본적으로 여전히 현재 프로젝트만 보여줍니다.

그런 다음 리시버를 실행합니다:

```sh
hatel serve            # 라이브 뷰, 현재 프로젝트만
hatel serve --all      # 이 컬렉터를 공유하는 모든 프로젝트
```

리시버는 단일 라이터 데몬입니다: `127.0.0.1:<port>`에 바인딩하므로 같은 포트의 두 번째
인스턴스는 종료되고, 비용 스냅샷은 정확히 하나의 라이터만 갖습니다. 리시버는 **항상 `200`**을
답합니다 — 상태코드는 본문이 *수신*되었음을 뜻하지 이 빌드가 디코딩할 수 있었는지를 뜻하지
않습니다. 그래서 로컬 뷰가 못 읽는 본문의 raw tee도 성공하고, OTLP 클라이언트는 재시도하지
않습니다(재시도는 delta 카운트를 부풀립니다). 디코딩 불가 본문은 stderr와 `doctor`(settings에서
프로토콜 오설정을 검출)로만 신호하며 상태코드로는 알리지 않습니다. 요청 본문을 64 MB로 제한하고,
손상된 락은 크래시 대신 복구합니다. 영속 상태는 보존 기간(`HATEL_RETENTION_DAYS`)으로 제한됩니다.
메모리 상의 세션별 누산기는 프로세스 시작 이후 관측된 세션 수로 제한되며, 정상적으로 재시작하는
데몬은 이를 작게 유지합니다.

## 다른 컬렉터로 전달(export)

리시버는 수신한 것을 하나 이상의 하류 OTLP/HTTP 컬렉터로 전달할 수 있습니다. 더 이상 hatel **이냐**
기업 컬렉터 **이냐**를 택하지 않고, hatel이 앞단에 앉아 기업 컬렉터로 tee합니다. 이는 OpenTelemetry
Collector 파이프라인을 따른 것으로, 리시버가 로컬에서 디코딩하며 하류로 전달하고, Prometheus/Honeycomb
등으로의 fan-out은 하류 컬렉터의 몫입니다 — 그래서 hatel은 **OTLP만** 내보냅니다.

대상은 `config.toml`(`$HATEL_CONFIG`, 없으면 `<config-dir>/hatel/config.toml`)에 설정합니다. 각
`[[export]]`는 하나의 대상과 그 길에 적용할 transform입니다:

```toml
[[export]]
endpoint = "http://collector.corp:4318"   # /v1/metrics, /v1/logs가 덧붙음
mode = "enriched"                           # raw | enriched
headers = { authorization = "…" }           # 예: 하류 인증(값은 절대 로깅 안 함)
exclude_projects = ["scratch"]              # 이 프로젝트만 빼고 전달…
# projects = ["work-a", "work-b"]           # …또는 이 프로젝트만 허용(둘 다는 불가)
# timeout_ms = 5000

[[export]]
endpoint = "http://archive:4318"
mode = "raw"
```

- **`raw`** — 들어온 OTLP를 byte-verbatim 전달(프로토콜 무관, `protobuf` 본문도 tee).
- **`enriched`** — 각 datapoint/record에 `project` 라벨(`session.id`로 조인)을 주입해, raw OTel이
  구조적으로 못 주는 프로젝트 단위 귀속을 하류 백엔드가 얻습니다. transform에는 `http/json` 스트림이
  필요하며, 세션이 미상인 datapoint는 그대로 전달합니다(라벨을 절대 날조하지 않음).
- **`projects`** / **`exclude_projects`** — 특정 프로젝트를 대상에서 제외합니다. 허용 목록(이것만
  전달) 또는 제외 목록(이것만 빼고 전달) 중 하나. 배치의 프로젝트는 `session.id`로 조인하므로
  `http/json`이 필요하며, 프로젝트를 아직 확정할 수 없는 배치는 **fail-closed**(필터 대상으로 전달
  안 함) — 시작 레이스에서 개인 프로젝트가 회사 컬렉터로 새지 않습니다. 두 키 모두 없으면 모든
  프로젝트를 전달합니다.

A/B는 두 토글이 아니라 **대상별 transform**입니다: 같은 endpoint에 둘 다 보내면 delta 메트릭이
이중집계되므로, 중복 endpoint는 로드 시 거부합니다. 전달은 best-effort·fail-open입니다 — 느리거나
도달 불가한 하류가 수집을 막지 않고(큐가 압력 시 드롭·카운트), 재시도하지 않습니다.

> **Egress는 레다크션되지 않습니다.** `raw`/`enriched`는 전체 OTLP 본문을 호스트 밖으로 전달하며,
> hatel의 allow-list/해싱은 hook ledger에만 적용되고 이 egress에는 적용되지 않습니다. export가 설정되면
> `doctor`가 이를 상시 경고합니다. (단, hatel은 설계상 content-free라 prompt/tool 본문을 담지 않으므로,
> Claude Code의 raw OTel을 기업 컬렉터로 직접 쏘는 것보다는 여전히 안전합니다 — 그래도 호스트 밖입니다.)

### 기존 컬렉터 앞에 hatel 끼워넣기

export는 리시버에 도달한 것만 전달하므로 Claude Code의 엔드포인트가 hatel을 가리켜야 합니다. 지금
기업 컬렉터를 가리키고 있다면, 한 명령으로 그것을 캡처하고 Claude Code를 hatel로 repoint합니다 —
컬렉터를 유지한 채 hatel을 얻습니다:

```sh
hatel init --insert                 # 현재 엔드포인트를 enriched 타겟으로 캡처하고 CC를 repoint
hatel init --insert --mode raw      # …byte-verbatim으로 전달
```

캡처한 엔드포인트(와 인증 헤더)를 `config.toml`에 적고, 엔드포인트를 설정한 스코프를 repoint하며,
`doctor`로 검증합니다. 엔드포인트가 **managed로 고정**돼 있으면 repoint할 수 없습니다 — `doctor`가
이를 명시하고, 그때는 hook ledger만 사용 가능합니다.

## Claude Code 스킬

설치 스크립트는 `hatel` 스킬을 `~/.claude/skills/`에도 설치합니다(저장소를 클론하는
사람을 위해 `.claude/skills/hatel/`에 포함되어 있고, 각 릴리스 아카이브 안에서 버전이
고정됩니다). 이 스킬을 통해 Claude Code는 텔레메트리를 대신 연결하고, 빠진 부분을 진단하고,
`report --format json`으로 "이 프로젝트 비용은 얼마였나 / 어느 서브에이전트가 토큰을 가장 많이
쓰나"에 답하고, 커스텀 플러그인 Kind를 스캐폴딩할 수 있습니다 — 모두 여기 문서화된 동일한
바이너리를 통해서. 스킬은 대화가 텔레메트리에 관한 것일 때만 로드되므로 그 외에는 비용이 들지
않습니다.

## 명령어

| 명령어 | 용도 |
|---|---|
| `serve [--port 4318] [--all] [--project N]` | OTLP/HTTP 리시버 + 라이브 세션별 롤업. 서브에이전트가 실행되면 서브에이전트별 토큰/비용 분해를 포함. |
| `report [--window 30d] [--format md\|text\|json] [--project N] [--kind K] [--top K]` | 롤링 윈도우에 대해 집계 — 그룹별: 레코드 수와 각 Kind의 `measures` 합계 — 그리고 비용 스냅샷. 대시보드용 `--format json`; `--top 0`은 모든 그룹 표시; `--kind K`는 등록된 단일 Kind로 롤업을 한정하고 비용 스냅샷을 생략. |
| `init [--scope user\|project\|local] [--print] [--remove] [--insert [--mode raw\|enriched]]` | `settings.json`에 텔레메트리 env + 라이프사이클 훅을 연결(또는 해제) — 멱등적·비파괴적·원자적. `--print`는 managed settings용 블록을 출력. `--insert`는 기존 기업 엔드포인트를 export 타겟으로 캡처하고 Claude Code를 hatel로 repoint. |
| `service [--remove] [--print]` | 빈틈 없는 수집을 위해 리시버를 launchd/systemd 사용자 서비스로 설치/제거(`serve --all` 실행). `--print`는 유닛 출력. |
| `doctor` | 와이어링을 검증하고 정책상의 빈틈을 정직하게 보고(아래 참조). |
| `kinds [--json]` | 등록된 Kind 목록(core + 플러그인). |
| `emit <kind> [key=value...] [--json OBJ]` | 등록된 Kind에 도메인 신호 하나를 기록(필드 쌍, `--json`, 또는 stdin) — 커스텀 메트릭을 위한 프로그래밍 경로. |

## 저장소(Storage)

저장소의 양쪽 절반 모두 하나의 추상화(`HATEL_SINK`)를 거칩니다: 에미터는 싱크를 통해
쓰고, `report`는 동일한 백엔드를 통해 읽습니다 — 그래서 선택이 끝까지 정직합니다(리포트는 JSONL을
소비하는 것과 똑같이 SQLite를 소비합니다):

- `jsonl` (기본) — 상태 디렉터리 아래 Kind별 append-only 파일 하나, 10 MB에서 로테이트
  (`HATEL_ROTATE_BYTES`). Git 친화적, grep 가능, 의존성 0.
- `sqlite` — 임베디드, WAL, `(kind, ts)`로 인덱싱되어 윈도우 읽기가 저렴함. 리포트 윈도우는 전체
  히스토리를 스캔하는 대신 SQL에서 필터링됩니다.

상태는 XDG 상태 디렉터리(`~/.local/state/hatel` 또는 플랫폼 등가물)에 있으며,
`HATEL_STATE_DIR`로 재정의합니다. 세션 인덱스(`session_index.jsonl`)와 비용
스냅샷(`cost_snapshot.jsonl`)은 싱크와 무관하게 항상 거기에 기록됩니다 — 리시버는 프로젝트가 없는
OTel 데이터를 귀속시키기 위해 인덱스가 필요하고, 비용 스냅샷은 이벤트 스트림이 아니라 세션별
스냅샷이기 때문입니다.

**라이브 `serve` 뷰**와 세션 인덱스는 같은 이름의 두 저장소를 git 루트 경로(프로젝트 *키*)로
구분합니다. 저장된 이벤트 레코드와 `report`는 사람이 읽는 *레이블*(basename)만 운반합니다 —
경로가 인덱스를 벗어나지 않도록 의도적으로 — 그래서 `report --project foo`는 레이블로 매칭하며 서로
다른 두 `foo` 저장소를 병합할 수 있습니다. `report --project`는 또한 `project` 필드가 없는
Kind(이를 포함하지 않은 `emit` Kind)를 에러가 아니라 행 없음으로 드롭합니다.

### 환경 변수

| 변수 | 효과 |
|---|---|
| `HATEL_SINK` | `jsonl` (기본) / `sqlite` |
| `HATEL_STATE_DIR` | 상태 디렉터리 재정의 |
| `HATEL_CONFIG` | `config.toml` 경로 재정의(`[[export]]` 대상) |
| `HATEL_PLUGINS` | 플러그인 TOML 경로, OS 경로 구분자(`:` Unix, `;` Windows) |
| `HATEL_ROTATE_BYTES` | JSONL 로테이션 임계값(기본 10 MB) |
| `HATEL_RETENTION_DAYS` | 비용 스냅샷 보존 기간(기본 90, 최대 100000) |
| `HATEL_DISABLED=1` | 훅을 no-op으로 전환 |
| `HATEL_STRICT=1` | 허용 목록 밖의 페이로드 키에 대해 (조용히 드롭하지 않고) 에러 |
| `HATEL_TESTING=1` | 쓰기를 `_test/` 하위 디렉터리로 리다이렉트 |

이들은 수집기 자체를 설정하며 `settings.json`에 있는 Claude Code의 `OTEL_*` 텔레메트리 설정과는
무관합니다.

## 확장: 프로젝트별 커스텀 메트릭

플러그인은 TOML 스키마 파일입니다 — 코드 없음, 재컴파일 없음. core가 사용하는 동일한 로더를 통해
Kind(그리고 선택적으로 훅 바인딩)를 기여합니다. `HATEL_PLUGINS=path/to/plugin.toml`로
가리킵니다(여럿이면 OS 경로 구분자). Kind마다:

- `fields` — 단일 허용 목록(이외의 것은 쓰기 전에 드롭됨).
- `group_key` — 리포트가 그룹화하는 필드.
- `measures` — 리포트가 그룹별로 **합산**하는 숫자 필드(소요 시간, 카운트, 비용). 첫 번째가 그룹
  순위를 매기는 기본 메트릭. 숫자 문자열은 숫자와 동일하게 합산되므로, `emit`에서 `=` 대신 `:=`로
  잘못 써도 measure가 조용히 0이 되지 않습니다.
- `redact` — 저장 전에 해싱되는 필드.

중복 Kind 이름은 시작 시 하드 에러이므로, core의 평면 이름(`tool`, `prompt`)과 충돌하지 않도록
플러그인 Kind에 네임스페이스를 부여하세요(`team.deploy`). Kind 이름은 `[A-Za-z0-9._-]`로 제한됩니다
(원장 파일명이기도 함).

커스텀 Kind은 두 경로 중 하나로 채워집니다. **신호가 어디서 발생하는지로 선택**하고, **Kind당
라이터를 하나로** 유지하세요(두 경로 모두로 쓰인 Kind은 이중 계산됨):

- Claude Code 라이프사이클이 관측할 수 있는 신호 → **훅 바인딩**(코드 0, 세션의 프로젝트로 자동
  귀속).
- 프로젝트 자체 로직만 아는 신호 → **`emit`**(당신의 도구가 기록함. 수집기는 관측할 수 없음).

**1. 훅 바인딩** — Claude Code 라이프사이클 이벤트에서 도출 가능한 신호를 코드 없이:

```toml
[[kind]]
name = "team.deploy"
fields = ["session_id", "project", "service", "ok"]
group_key = "service"

[[binding]]
event = "PostToolUse"
kind = "team.deploy"
map.session_id = { from = "session_id" }
map.service   = { from = "tool_name" }
map.ok        = { from = "tool_response", present = true }
```

필드 맵 변환: `from`(통과; 리스트 `["a","b"]`는 순서대로 시도하여 버전에 민감한 필드명을 허용),
`capture`(정규식 그룹 1), `len`(문자열 길이), `present`(필드 존재 → bool), `basename`(마지막 경로
구성요소), `const`. 적용되지 않는 변환은 필드를 생략합니다 — 절대 조작하지 않음. 바인딩이
`git_branch`에서 매핑할 때(그리고 그때만) 훅은 `.git/HEAD`에서 그것을 읽으므로(서브프로세스 없음),
프로젝트는 코드 없이 spec 슬러그를 도출할 수 있습니다:
`map.spec_slug = { from = "git_branch", capture = "^spec/(.+)$" }`.

**2. `emit`** — Claude Code 이벤트가 *아닌* 도메인 신호(spec 게이트 결정, 규칙 검사 롤업, 배포
결과). 당신의 도구가 직접 기록합니다:

```sh
# 필드 쌍 — key=value는 문자열, key:=value는 JSON(숫자, 불리언, 배열)
hatel emit ci_check check=lint date=2026-06-09 runs:=14000 failures:=3
# 또는 --json으로 JSON 객체 전체, 또는 stdin으로 파이프
echo '{"check":"lint","runs":14000}' | hatel emit ci_check
```

`key=` / `key:=` 구분은 타입을 명시적으로 유지합니다 — 문자열에서 추측하지 않습니다. `emit`은
Kind을 검증하고, 동일한 허용 목록과 리댁션을 적용하며, 활성 싱크를 통해 씁니다. Kind이 받지 않는
필드는 드롭되지만(허용 목록) **허용 필드 목록과 함께 stderr로 경고**되므로, 오타가 조용히 사라지지
않고 즉시 드러납니다. 알 수 없는 Kind이나 잘못된 입력은 0이 아닌 코드로 종료하고(호출자는 기록되지
않았음을 알게 됨), IO 에러는 fail-open을 유지합니다. 언어 무관 — 어떤 언어의 어떤 프로젝트든
바이너리를 호출합니다. `hatel kinds`로 등록된 모든 Kind과 정확한 필드를 확인하세요.

훅과 달리 `emit`은 작업 디렉터리에서 프로젝트를 **추론하지 않습니다**: 에미터 프로세스(스케줄러,
CI 잡)는 어디서든 실행될 수 있어 추측은 오귀속을 낳기 때문입니다. 교차 프로젝트 분석을 위해서는
원하는 귀속(`project`, 슬러그, 조직)을 페이로드의 필드로 포함하세요 — 호출자는 그것을 알고,
수집기는 모릅니다. `plugins/example.toml`은 동작하는 예시입니다: 당신의 도구가 기록하는 CI 검사
롤업과, 코드 없는 브랜치 귀속 Kind이 함께 있습니다.

## 프라이버시

- 허용 목록이 1차 방어선입니다. core는 콘텐츠를 담는 필드를 **전혀** 제공하지 않습니다. 프롬프트는
  길이를, 도구는 이름을 저장합니다 — 텍스트나 인자는 절대 아님. 이는 Claude Code 자체의 기본 비활성
  `OTEL_LOG_USER_PROMPTS` / `OTEL_LOG_TOOL_DETAILS`를 그대로 반영합니다.
- `redact` 필드는 쓰기 전에 해싱됩니다(BLAKE3, 16 hex 문자).
- 이벤트 레코드는 프로젝트 **레이블**만 운반합니다. 절대 git 루트 경로는 로컬 세션 인덱스에만
  존재합니다.
- 모든 것은 당신의 머신에 남습니다. 실패는 fail-open입니다: 쓰기 에러는 stderr 노트로 격하될 뿐
  도구 호출을 절대 막지 않습니다.

## 상시 수집 (빈틈 없음)

네이티브 OTel은 push 전용입니다: 토큰과 비용은 리시버가 실행 중일 때만 캡처됩니다. 빈틈 없는
사용자 레벨 수집을 위해 백그라운드 서비스로 설치하세요 — `hatel`이 유닛 파일을 작성하고
로드합니다(macOS는 launchd LaunchAgent, Linux는 systemd `--user` 유닛):

```sh
hatel service           # 설치 + 시작: `serve --all` 실행, 로그인/실패 시 유지·재시작
hatel service --remove  # 중지 후 제거
hatel service --print   # 설치 대신 유닛 출력 — 검토하거나 MDM에 전달용
```

유닛은 설치한 바로 그 바이너리를 실행하므로, `cargo install`이나 `--bin-dir` 이동 후
`hatel service`를 다시 실행하면 경로가 갱신됩니다. (`scripts/install.sh --service`는 설치와 같은
단계에서 이 작업을 수행합니다.)

## 엔터프라이즈 / managed settings

수집기는 managed 정책과 절대 싸우지 않습니다. 적응합니다:

- **OTel이 사내 컬렉터로 재지정됨** — 로컬 훅 원장은 계속 동작합니다. `session.id` 결합은 네이티브
  데이터가 어디에 도착하든 유지되므로, 메트릭은 사내 백엔드에서 질의되고 세션 기준으로 로컬 도메인
  원장과 결합됩니다.
- **`allowManagedHooksOnly`** — user/project 훅이 차단되므로, IT가 `hatel-hook`을
  *managed* 훅으로 배포합니다(단일 정적 바이너리를 MDM으로 배포). `doctor`는 파일 기반 managed
  settings(macOS / Linux / Windows 경로)에서 이를 감지합니다. 비파일 managed 소스(Windows 레지스트리,
  MDM 서버 드롭인)는 읽지 않습니다.
- **`OTEL_METRICS_INCLUDE_SESSION_ID=false`** — 세션별 귀속이 불가능해집니다. `doctor`는 이를 있는
  그대로 보고하고, 조직/사용자 집계는 여전히 동작합니다. 추측된 폴백은 없습니다 — 사용할 수 없는
  신호는 사용 불가로 보고될 뿐, 절대 조작되지 않습니다.

## 레이아웃

```
crates/core   비동기 없는 라이브러리: model, registry, schema, pii, sinks, session, hook, report
crates/hook   가벼운 훅 바이너리 (core만)
crates/cli    리시버, 리포트, doctor (core + tokio/axum)
plugins/      예시 선언적 플러그인
```
