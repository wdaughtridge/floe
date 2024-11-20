defmodule Floe.Application do
  # See https://hexdocs.pm/elixir/Application.html
  # for more information on OTP Applications
  @moduledoc false

  use Application

  @impl true
  def start(_type, _args) do
    children = [
      FloeWeb.Telemetry,
      Floe.Repo,
      {DNSCluster, query: Application.get_env(:floe, :dns_cluster_query) || :ignore},
      {Phoenix.PubSub, name: Floe.PubSub},
      # Start the Finch HTTP client for sending emails
      {Finch, name: Floe.Finch},
      # Start a worker by calling: Floe.Worker.start_link(arg)
      # {Floe.Worker, arg},
      # Start to serve requests, typically the last entry
      FloeWeb.Endpoint
    ]

    # See https://hexdocs.pm/elixir/Supervisor.html
    # for other strategies and supported options
    opts = [strategy: :one_for_one, name: Floe.Supervisor]
    Supervisor.start_link(children, opts)
  end

  # Tell Phoenix to update the endpoint configuration
  # whenever the application is updated.
  @impl true
  def config_change(changed, _new, removed) do
    FloeWeb.Endpoint.config_change(changed, removed)
    :ok
  end
end
