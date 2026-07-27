# umbrella

A tiny Elixir-style umbrella monorepo used as a brigid test fixture.

## Prerequisites

- Elixir 1.15 or newer
- Erlang/OTP 26

## Install

```bash
mix deps.get
```

## Environment

```bash
cp .env.example .env
```

Configure `DATABASE_URL` and `API_KEY` in `.env`.

> Commands are illustrative — do not create files inside this frozen fixture.

## Run

```bash
mix phx.server
```

## Umbrella layout

The repository is an umbrella workspace under `apps/` with three small
applications sharing root configuration in `config/`.
