defmodule FloeWeb.ApiController do
  use FloeWeb, :controller

  def whip(conn, params) do
    stream_id = params["stream_id"]

    # we expect the offer to only be SENDING data
    {:ok, sdp_offer, conn} = Plug.Conn.read_body(conn)

    {:ok, link} = Floe.SFU.start_link()

    :ok = GenServer.cast(Floe.Registry, {:insert, stream_id, link})
    {:ok, sdp_answer} = Floe.SFU.put_new_whip_client(sdp_offer, link)

    conn
    |> Plug.Conn.put_resp_header("Content-Type", "application/sdp")
    |> Plug.Conn.put_resp_header("Location", "/api/resource/" <> stream_id)
    |> Plug.Conn.send_resp(201, sdp_answer)
  end

  def whep(conn, params) do
    stream_id = params["stream_id"]

    # client is requesting to only be RECEIVING data, so we should
    # see if there is already a stream started for the requested
    # stream id
    {:ok, sdp_offer, conn} = Plug.Conn.read_body(conn)

    response = GenServer.call(Floe.Registry, {:lookup, stream_id})
    link = response[:stream_handle]

    {:ok, sdp_answer} = Floe.SFU.put_new_whep_client(sdp_offer, link)

    conn
    |> Plug.Conn.put_resp_header("Content-Type", "application/sdp")
    |> Plug.Conn.put_resp_header("Location", "/api/resource/" <> stream_id)
    |> Plug.Conn.send_resp(201, sdp_answer)
  end

  def resource(conn, params) do
    stream_id = params["stream_id"]

    {:ok, trickle_ice, conn} = Plug.Conn.read_body(conn)

    response = GenServer.call(Floe.Registry, {:lookup, stream_id})
    link = response[:stream_handle]

    :ok = Floe.SFU.put_new_remote_candidate(trickle_ice, link)

    conn
    |> Plug.Conn.resp(204, "")
    |> Plug.Conn.send_resp()
  end
end
