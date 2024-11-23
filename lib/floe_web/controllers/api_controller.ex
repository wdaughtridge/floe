defmodule FloeWeb.ApiController do
  use FloeWeb, :controller

  def whip(conn, _params) do
    # We expect the offer to only be SENDING data.
    {:ok, sdp_offer, conn} = Plug.Conn.read_body(conn)

    {:ok, link} = Floe.SFU.start_link()
    :ok = GenServer.cast(Floe.Registry, {:insert, "12345", link})
    {:ok, sdp_answer} = Floe.SFU.put_new_whip_client(sdp_offer, link)

    conn
    |> Plug.Conn.put_resp_header("location", "/resource/12345")
    |> Plug.Conn.put_resp_header("content-type", "application/sdp")
    |> Plug.Conn.send_resp(201, sdp_answer)
  end

  def whep(conn, _params) do
    # Client is requesting to only be RECEIVING data, so we should
    # see if there is already a stream started for the requested
    # stream id.
    {:ok, sdp_offer, conn} = Plug.Conn.read_body(conn)

    [response | _rest] = GenServer.call(Floe.Registry, {:lookup, "12345"})
    {stream_id, link} = response
    IO.inspect(stream_id)
    IO.inspect(link)
    {:ok, sdp_answer} = Floe.SFU.put_new_whep_client(sdp_offer, link)

    conn
    |> Plug.Conn.put_resp_header("location", "/resource/12345")
    |> Plug.Conn.put_resp_header("content-type", "application/sdp")
    |> Plug.Conn.send_resp(201, sdp_answer)
  end
end
