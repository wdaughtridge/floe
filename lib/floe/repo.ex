defmodule Floe.Repo do
  use Ecto.Repo,
    otp_app: :floe,
    adapter: Ecto.Adapters.Postgres
end
