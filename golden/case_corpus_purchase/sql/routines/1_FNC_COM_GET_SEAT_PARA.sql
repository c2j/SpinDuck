CREATE OR REPLACE FUNCTION FNC_COM_GET_SEAT_PARA (p_i_date            varchar2,
                                                 p_i_market_code     varchar2,
                                                 p_i_fund_code       varchar2,
                                                 p_i_seat_code       varchar2,
                                                 p_i_stock_kind      varchar2,
                                                 p_o_return_flag     out varchar2,
                                                 p_o_return_rate_org out number,
                                                 p_o_return_rate     out number)
  return VARCHAR is

  v_commision_type     par_fund_seat_para.commision_type%type := '';
  v_commision_fee_type par_fund_seat_para.commision_fee_type%type := '';
  v_all_stock_kind     par_sys_marketrate.stock_kind%type := '';

begin
  v_all_stock_kind := substr(p_i_stock_kind, 1, 2) || '00';

  begin

    select commision_type,
           commision_fee_type,
           return_rate_flag,
           return_rate_org,
           return_rate
      into v_commision_type,
           v_commision_fee_type,
           p_o_return_flag,
           p_o_return_rate_org,
           p_o_return_rate
      from (select commision_type,
                   commision_fee_type,
                   return_rate_flag,
                   return_rate_org,
                   return_rate
              from par_fund_seat_para
             where fund_code = p_i_fund_code
               and market_code = p_i_market_code
               and seat_code = p_i_seat_code
               and (stock_kind = p_i_stock_kind or
                   stock_kind = v_all_stock_kind or stock_kind = '0000')
               and p_i_date between inure_begin_date and inure_end_date
             order by stock_kind desc)
     where rownum = 1;
  exception
    when OTHERS then
      begin
        select commision_type,
               commision_fee_type,
               return_rate_flag,
               return_rate_org,
               return_rate
          into v_commision_type,
               v_commision_fee_type,
               p_o_return_flag,
               p_o_return_rate_org,
               p_o_return_rate
          from (select commision_type,
                       commision_fee_type,
                       return_rate_flag,
                       return_rate_org,
                       return_rate
                  from par_fund_seat_para
                 where fund_code = p_i_fund_code
                   and market_code = p_i_market_code
                   and seat_code = '000000'
                   and (stock_kind = p_i_stock_kind or
                       stock_kind = v_all_stock_kind or stock_kind = '0000')
                   and p_i_date between inure_begin_date and inure_end_date
                 order by stock_kind desc)
         where rownum = 1;
      exception
        when OTHERS then
          v_commision_type     := 1;
          v_commision_fee_type := 1;
          p_o_return_flag      := '0';
          p_o_return_rate_org  := 0;
          p_o_return_rate      := 0;
      end;
  end;

  return v_commision_type || v_commision_fee_type;

end ;
