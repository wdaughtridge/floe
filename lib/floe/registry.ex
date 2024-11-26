defmodule Floe.Registry do
  use GenServer

  def start_link(opts) do
    server = opts[:name]
    GenServer.start_link(__MODULE__, server, opts)
  end

  @impl true
  def init(_params) do
    :ok = :syn.add_node_to_scopes([:streams])
    {:ok, %{}}
  end

  @impl true
  def handle_call({:lookup, stream_id}, _from, state) do
    {_pid, stream_info} = :syn.lookup(:streams, stream_id)
    {:reply, stream_info, state}
  end

  @impl true
  def handle_cast({:insert, stream_id, stream_handle}, state) do
    :syn.register(:streams, stream_id, self(), [stream_handle: stream_handle])
    {:noreply, state}
  end

  @impl true
  def handle_info(_msg, state) do
    {:noreply, state}
  end
end
