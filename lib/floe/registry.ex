defmodule Floe.Registry do
  use GenServer

  def start_link(opts) do
    server = opts[:name]
    GenServer.start_link(__MODULE__, server, opts)
  end

  @impl true
  def init(_params) do
    :ets.new(:streams, [:named_table, read_concurrency: true])
    {:ok, %{}}
  end

  @impl true
  def handle_call({:lookup, stream_id}, _from, state) do
    stream_info = :ets.lookup(:streams, stream_id)
    {:reply, stream_info, state}
  end

  @impl true
  def handle_cast({:insert, stream_id, stream_handle}, state) do
    :ets.insert(:streams, {stream_id, stream_handle})
    {:noreply, state}
  end

  @impl true
  def handle_info(_msg, state) do
    {:noreply, state}
  end
end
